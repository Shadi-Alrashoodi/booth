use std::collections::HashMap;
use std::f64::consts::PI;

use voice::mix::{FrameSource, Mixer, StereoSource};
use voice::stereo::buffer::{
    Buffer, BufferStats, MAX_DEPTH_MS, Pulled, QUIET_RMS, SoundPacket, Source,
};
use voice::stereo::{CHANNELS, Decoder, Encoder, FRAME, MAX_PACKET, StereoError};

// One pull, 10 ms of play, in frames at 48 kHz: the unit the listener below
// works in.
const TICK: u64 = FRAME as u64;
const SECOND: u64 = 48_000;
const SAMPLES: usize = FRAME * CHANNELS;

// No step between two output samples may be larger than this, joins
// included: concealment, the fades, frames added and dropped. The test tones
// at 0.3 step at most 0.029 after the codec; a fade from silence straight
// after one concealed frame stepped 0.28.
const MAX_JOIN_STEP: f32 = 0.04;

#[derive(Clone)]
struct Sent {
    seq: u16,
    frame: Vec<u8>,
    first: bool,
    last: bool,
    // When the frame was complete and left the sharer, in frames.
    sent_at: u64,
}

impl Sent {
    fn packet(&self) -> SoundPacket<'_> {
        SoundPacket {
            seq: self.seq,
            frame: &self.frame,
            first: self.first,
            last: self.last,
        }
    }
}

// Interleaved, left first, at 0.3 of full scale.
fn tones(seconds: f64, left_hz: f64, right_hz: f64) -> Vec<f32> {
    let frames = (seconds * SECOND as f64).round() as usize;
    (0..frames)
        .flat_map(|i| {
            let t = i as f64 / SECOND as f64;
            [
                (0.3 * (2.0 * PI * left_hz * t).sin()) as f32,
                (0.3 * (2.0 * PI * right_hz * t).sin()) as f32,
            ]
        })
        .collect()
}

// 440 Hz on the left and 660 Hz on the right: sound that never goes quiet.
fn music(seconds: f64) -> Vec<f32> {
    tones(seconds, 440.0, 660.0)
}

fn silence(seconds: f64) -> Vec<f32> {
    vec![0.0; (seconds * SECOND as f64).round() as usize * CHANNELS]
}

// The tones in bursts of 150 ms, which start and end on a zero crossing,
// each followed by 50 ms of silence.
fn bursts(seconds: f64) -> Vec<f32> {
    let mut sound = Vec::new();
    while (sound.len() as f64) < seconds * (SECOND as usize * CHANNELS) as f64 {
        sound.extend(music(0.15));
        sound.extend(silence(0.05));
    }
    sound
}

// Whether frame `n` of bursts() is silence.
fn in_a_gap(n: usize) -> bool {
    n % 20 >= 15
}

// One spell: `sound` through an encoder from reset, numbered on from
// `first_seq`, its first frame marked first. Frame n is complete at `start`
// plus n + 1 ticks.
fn spell(sound: &[f32], first_seq: u16, start: u64) -> Vec<Sent> {
    let mut encoder = Encoder::new().unwrap();
    let mut out = [0u8; MAX_PACKET];
    let (frames, _) = sound.as_chunks::<SAMPLES>();
    frames
        .iter()
        .enumerate()
        .map(|(n, pcm)| {
            let len = encoder.encode(pcm, &mut out).unwrap();
            Sent {
                seq: first_seq.wrapping_add(n as u16),
                frame: out[..len].to_vec(),
                first: n == 0,
                last: false,
                sent_at: start + (n as u64 + 1) * TICK,
            }
        })
        .collect()
}

fn on_time(sent: &[Sent]) -> Vec<Option<u64>> {
    sent.iter().map(|packet| Some(packet.sent_at)).collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ear {
    Pulling,
    // The render thread missed its turn: no pull, and nobody says why.
    Stalled,
    // The mixer stopped pulling and says so.
    Deafened,
}

fn always(_: u64) -> Ear {
    Ear::Pulling
}

// What came out of one pull, and when. Pulls where the buffer had nothing
// are kept as silence, since that is what the mixer plays for this stream.
struct Heard {
    at: u64,
    pulled: Option<Pulled>,
    audio: Vec<f32>,
}

// The render side of one stream: a pull every 10 ms. Before each pull it
// pushes every frame that has arrived by then, oldest arrival first.
struct Listener {
    buffer: Buffer,
    now: u64,
    pushed_through: Option<u64>,
    heard: Vec<Heard>,
}

impl Listener {
    fn new() -> Listener {
        Listener {
            buffer: Buffer::new().unwrap(),
            now: 0,
            pushed_through: None,
            heard: Vec::new(),
        }
    }

    fn run(&mut self, sent: &[Sent], arrive: &[Option<u64>], until: u64, ear: impl Fn(u64) -> Ear) {
        let mut order: Vec<(u64, usize)> = arrive
            .iter()
            .enumerate()
            .filter_map(|(i, at)| Some(((*at)?, i)))
            .collect();
        order.sort_unstable();
        let mut frame = [0f32; SAMPLES];
        while self.now < until {
            let done = self.pushed_through;
            let from = order.partition_point(|&(at, _)| done.is_some_and(|done| at <= done));
            let to = order.partition_point(|&(at, _)| at <= self.now);
            for &(_, i) in &order[from..to.max(from)] {
                self.buffer.push(sent[i].packet()).unwrap();
            }
            self.pushed_through = Some(self.now);
            match ear(self.now) {
                Ear::Pulling => {
                    let pulled = self.buffer.pull(&mut frame);
                    if let Some(pulled) = pulled {
                        assert_eq!(pulled.frames, FRAME);
                    }
                    let audio = match pulled {
                        Some(_) => frame.to_vec(),
                        None => vec![0.0; SAMPLES],
                    };
                    self.heard.push(Heard {
                        at: self.now,
                        pulled,
                        audio,
                    });
                }
                Ear::Stalled => {}
                Ear::Deafened => self.buffer.pause(),
            }
            self.now += TICK;
        }
    }

    fn stats(&self) -> BufferStats {
        self.buffer.stats()
    }

    fn played_at(&self, seq: u16) -> Option<u64> {
        self.played_from(seq, 0)
    }

    // When `seq` played at `from` or later, for numbers two spells share.
    fn played_from(&self, seq: u16, from: u64) -> Option<u64> {
        self.heard
            .iter()
            .filter(|heard| heard.at >= from)
            .find(|heard| heard.pulled.map(|pulled| pulled.source) == Some(Source::Packet(seq)))
            .map(|heard| heard.at)
    }

    // The pull at `at`, as an index into `heard`.
    fn pull_at(&self, at: u64) -> usize {
        self.heard.iter().position(|heard| heard.at == at).unwrap()
    }

    fn sources(&self, pulls: std::ops::Range<usize>) -> Vec<Option<Source>> {
        self.heard[pulls]
            .iter()
            .map(|heard| heard.pulled.map(|pulled| pulled.source))
            .collect()
    }

    // Indices into `sent` of the frames that never played.
    fn skipped(&self, sent: &[Sent]) -> Vec<usize> {
        (0..sent.len())
            .filter(|&i| self.played_at(sent[i].seq).is_none())
            .collect()
    }

    // The largest step between neighbouring samples of one channel wherever
    // the output was continuous (a tick without a pull breaks it).
    fn largest_step(&self) -> (f32, u64) {
        let mut largest = (0.0f32, 0);
        let mut before: Option<&[f32; CHANNELS]> = None;
        let mut end = None;
        for heard in &self.heard {
            if end != Some(heard.at) {
                before = None;
            }
            let (pairs, _) = heard.audio.as_chunks::<CHANNELS>();
            for (k, pair) in pairs.iter().enumerate() {
                if let Some(before) = before {
                    for (sample, last) in pair.iter().zip(before) {
                        if (sample - last).abs() > largest.0 {
                            largest = ((sample - last).abs(), heard.at + k as u64);
                        }
                    }
                }
                before = Some(pair);
            }
            end = Some(heard.at + TICK);
        }
        largest
    }

    fn assert_no_clicks(&self) {
        let (step, at) = self.largest_step();
        assert!(
            step <= MAX_JOIN_STEP,
            "step of {step:.3} at {:.1} ms is over {MAX_JOIN_STEP}",
            at as f64 / 48.0
        );
    }

    // Every frame that played came after the one before it in its spell.
    fn assert_in_order(&self) {
        let mut before: Option<u16> = None;
        for heard in &self.heard {
            let Some(Source::Packet(seq)) = heard.pulled.map(|pulled| pulled.source) else {
                if heard.pulled.is_none() {
                    before = None;
                }
                continue;
            };
            if let Some(before) = before {
                assert!(
                    seq.wrapping_sub(before) as i16 > 0,
                    "seq {seq} played after {before} at {:.1} ms",
                    heard.at as f64 / 48.0
                );
            }
            before = Some(seq);
        }
    }
}

// What the buffer's decoder gave at each pull, replayed from where each pull
// says its frame came from: a packet is decoded, a made-up frame and the end
// are concealed, silence is nothing. A frame left out between two packets
// played one after the other was dropped, and decoded just before the second.
struct Replayed {
    frame: Vec<f32>,
    dropped: Option<Vec<f32>>,
}

fn replay(sent: &[Sent], heard: &[Heard]) -> Vec<Replayed> {
    fn decode(decoder: &mut Decoder, packet: &Sent) -> Vec<f32> {
        let mut out = vec![0.0; SAMPLES];
        assert_eq!(decoder.decode(&packet.frame, &mut out).unwrap(), FRAME);
        out
    }
    let packets: HashMap<u16, &Sent> = sent.iter().map(|packet| (packet.seq, packet)).collect();
    let mut decoder = Decoder::new().unwrap();
    let mut before: Option<u16> = None;
    heard
        .iter()
        .map(|heard| {
            let mut frame = vec![0.0; SAMPLES];
            match heard.pulled.map(|pulled| pulled.source) {
                Some(Source::Packet(seq)) => {
                    if packets[&seq].first {
                        decoder.reset().unwrap();
                    }
                    let skipped = seq.wrapping_sub(1);
                    let dropped = (before == Some(seq.wrapping_sub(2)))
                        .then(|| decode(&mut decoder, packets[&skipped]));
                    before = Some(seq);
                    return Replayed {
                        frame: decode(&mut decoder, packets[&seq]),
                        dropped,
                    };
                }
                Some(Source::Concealed | Source::End) => {
                    assert_eq!(decoder.conceal(&mut frame).unwrap(), FRAME);
                }
                Some(Source::Silence) | None => {}
            }
            before = None;
            Replayed {
                frame,
                dropped: None,
            }
        })
        .collect()
}

// The buffer's fade in from silence, over the frame's first 2.5 ms.
fn faded_in(frame: &[f32]) -> Vec<f32> {
    let mut out = frame.to_vec();
    let (pairs, _) = out.as_chunks_mut::<CHANNELS>();
    for (i, pair) in pairs.iter_mut().take(120).enumerate() {
        let gain = (i as f32 + 1.0) / 120.0;
        pair[0] *= gain;
        pair[1] *= gain;
    }
    out
}

// `from` blended into the start of `into` over `len` frames, as the buffer
// blends a dropped frame into the one after it.
fn blended(from: &[f32], into: &[f32], len: usize) -> Vec<f32> {
    let mut out = into.to_vec();
    for i in 0..len {
        let gain = (i as f32 + 1.0) / (len as f32 + 1.0);
        for c in 0..CHANNELS {
            let at = CHANNELS * i + c;
            out[at] = from[at] * (1.0 - gain) + out[at] * gain;
        }
    }
    out
}

fn rms(frame: &[f32], channel: usize) -> f32 {
    let (sum, n) = frame
        .iter()
        .skip(channel)
        .step_by(CHANNELS)
        .fold((0.0, 0), |(sum, n), s| (sum + s * s, n + 1));
    (sum / n as f32).sqrt()
}

// The level at `hz` of one channel of interleaved stereo, in dB of full
// scale.
fn level_db(stereo: &[f32], channel: usize, hz: f64) -> f64 {
    let samples: Vec<f64> = stereo
        .iter()
        .skip(channel)
        .step_by(CHANNELS)
        .map(|&s| f64::from(s))
        .collect();
    let n = samples.len() as f64;
    let (mut re, mut im, mut sum) = (0.0, 0.0, 0.0);
    for (i, s) in samples.iter().enumerate() {
        let w = 0.5 - 0.5 * (2.0 * PI * i as f64 / (n - 1.0)).cos();
        let angle = 2.0 * PI * hz * i as f64 / 48_000.0;
        re += s * w * angle.cos();
        im += s * w * angle.sin();
        sum += w;
    }
    let amplitude = 2.0 * (re * re + im * im).sqrt() / sum;
    20.0 * amplitude.max(1e-9).log10()
}

#[test]
fn frames_in_order_play_in_order_from_one_frame_deep() {
    let sent = spell(&music(1.0), 65_500, 0);
    let mut listener = Listener::new();
    listener.run(&sent, &on_time(&sent), SECOND + TICK, always);
    // Each frame plays at the first pull after it arrives, through the wrap
    // of the numbering.
    assert!(sent.iter().any(|packet| packet.seq == 0));
    for packet in &sent {
        assert_eq!(
            listener.played_at(packet.seq),
            Some(packet.sent_at),
            "seq {}",
            packet.seq
        );
    }
    let stats = listener.stats();
    assert_eq!((stats.depth_frames, stats.depth_ms), (1, 10));
    assert_eq!((stats.late, stats.lost, stats.concealed), (0, 0, 0));
    assert_eq!((stats.inserted, stats.dropped), (0, 0));
    assert_eq!(stats.loss_percent, 0.0);
    // The first frame fades in over 2.5 ms; after it, what plays is what the
    // decoder gave.
    let replayed = replay(&sent, &listener.heard);
    let first = listener.pull_at(sent[0].sent_at);
    assert_eq!(
        listener.heard[first].audio,
        faded_in(&replayed[first].frame)
    );
    for (heard, replayed) in listener.heard[first + 1..]
        .iter()
        .zip(&replayed[first + 1..])
    {
        assert_eq!(heard.audio, replayed.frame);
    }
    listener.assert_in_order();
    listener.assert_no_clicks();
}

// The room makes the buffer off the render thread and hands it over.
#[test]
fn the_buffer_can_move_to_the_render_thread() {
    fn movable<T: Send>() {}
    movable::<Buffer>();
}

#[test]
fn only_what_booth_sends_is_taken() {
    let mut buffer = Buffer::new().unwrap();
    // A mono CELT frame of 10 ms.
    let refused = buffer
        .push(SoundPacket {
            seq: 7,
            frame: &[30 << 3, 1, 2],
            first: true,
            last: false,
        })
        .unwrap_err();
    assert_eq!(refused.seq, 7);
    assert!(matches!(refused.reason, StereoError::Mono));
    assert_eq!(
        refused.to_string(),
        "pc sound frame is mono; Booth sends stereo (seq 7)"
    );
    assert!(buffer.pull(&mut [0.0; SAMPLES]).is_none());
    assert!(!buffer.stats().playing);
}

// What the room will wrap the buffer in, here with a record of what played.
struct Stream {
    buffer: Buffer,
    takes_period: bool,
    sources: Vec<Source>,
}

impl StereoSource for Stream {
    fn next_frame(&mut self, out: &mut [f32]) -> Option<usize> {
        let pulled = self.buffer.pull(out)?;
        self.sources.push(pulled.source);
        Some(pulled.frames)
    }

    fn pause(&mut self) {
        self.buffer.pause();
    }

    fn set_period(&mut self, frames: usize) {
        if self.takes_period {
            self.buffer.set_period(frames);
        }
    }
}

enum Nobody {}

impl FrameSource for Nobody {
    fn next_frame(&mut self, _: &mut [f32]) -> Option<usize> {
        match *self {}
    }
}

// The mixer itself pulling, at a render period of 1056 frames: two or three
// frames at once, every 22 ms.
fn mixed(sent: &[Sent], takes_period: bool) -> Stream {
    let period = 1056;
    let mut mixer: Mixer<u8, Nobody, Stream> = Mixer::with_streams();
    mixer.set_period(period);
    mixer.add_stream(
        0,
        Stream {
            buffer: Buffer::new().unwrap(),
            takes_period,
            sources: Vec::new(),
        },
    );
    let mut out = vec![0f32; period * CHANNELS];
    let (mut next, mut now) = (0, 0);
    while now < 5 * SECOND {
        let stream = mixer.stream_mut(&0).unwrap();
        while next < sent.len() && sent[next].sent_at <= now {
            stream.buffer.push(sent[next].packet()).unwrap();
            next += 1;
        }
        mixer.render_stereo(&mut out);
        now += period as u64;
    }
    mixer.remove_stream(&0).unwrap()
}

#[test]
fn a_period_of_1056_frames_starts_three_deep() {
    let sent = spell(&music(5.0), 0, 0);
    let stream = mixed(&sent, true);
    let stats = stream.buffer.stats();
    assert_eq!((stats.depth_frames, stats.depth_ms), (3, 30));
    // Silence before the first frame, then every frame from its own packet:
    // three deep, the last frame a period takes is always in.
    let played: Vec<Source> = stream
        .sources
        .iter()
        .copied()
        .skip_while(|&source| source == Source::Silence)
        .collect();
    assert!(played.len() > 480, "{} played", played.len());
    let expected: Vec<Source> = sent
        .iter()
        .take(played.len())
        .map(|packet| Source::Packet(packet.seq))
        .collect();
    assert_eq!(played, expected);
    assert_eq!(
        (stats.late, stats.lost, stats.concealed),
        (0, 0, 0),
        "{stats:?}"
    );
    assert_eq!((stats.inserted, stats.dropped), (0, 0));

    // One frame deep, the same pulls find frames missing until the rules
    // have grown the buffer.
    let shallow = mixed(&sent, false);
    let made_up = shallow
        .sources
        .iter()
        .skip_while(|&&source| source == Source::Silence)
        .filter(|&&source| source == Source::Concealed)
        .count();
    println!("one frame deep at a period of 1056: {made_up} frames made up");
    assert!(made_up > 0);
}

// Straight after concealment the decoder blends the next frame in itself,
// over the 2.5 ms its frames overlap by, so that frame plays just as the
// decoder gives it. A fade from silence on top would dip to nothing and
// click.
#[test]
fn one_lost_frame_is_concealed_and_the_next_fades_in() {
    let sent = spell(&music(1.0), 0, 0);
    let mut arrive = on_time(&sent);
    arrive[50] = None;
    let mut listener = Listener::new();
    listener.run(&sent, &arrive, SECOND + TICK, always);
    let at = listener.pull_at(sent[50].sent_at);
    assert_eq!(
        listener.sources(at - 1..at + 2),
        [
            Some(Source::Packet(sent[49].seq)),
            Some(Source::Concealed),
            Some(Source::Packet(sent[51].seq))
        ]
    );
    let replayed = replay(&sent, &listener.heard);
    assert_eq!(listener.heard[at].audio, replayed[at].frame);
    assert_eq!(listener.heard[at + 1].audio, replayed[at + 1].frame);
    let stats = listener.stats();
    assert_eq!((stats.concealed, stats.lost, stats.late), (1, 1, 0));
    assert_eq!((stats.depth_frames, stats.inserted), (1, 0));
    assert!(stats.loss_percent > 0.0);
    listener.assert_no_clicks();
}

// Two frames of concealment, then a third faded out to nothing, and past
// that silence. The frame after it all fades in from silence over 2.5 ms.
#[test]
fn three_lost_fade_out_after_20_ms() {
    for lost in [3, 5] {
        let sent = spell(&music(1.0), 0, 0);
        let mut arrive = on_time(&sent);
        for when in &mut arrive[50..50 + lost] {
            *when = None;
        }
        let mut listener = Listener::new();
        listener.run(&sent, &arrive, SECOND + TICK, always);
        let at = listener.pull_at(sent[50].sent_at);
        let mut expected = vec![Some(Source::Concealed); 3];
        expected.extend(vec![Some(Source::Silence); lost - 3]);
        expected.push(Some(Source::Packet(sent[50 + lost].seq)));
        assert_eq!(listener.sources(at..at + lost + 1), expected, "{lost} lost");
        let replayed = replay(&sent, &listener.heard);
        assert_eq!(listener.heard[at + 1].audio, replayed[at + 1].frame);
        let third = &listener.heard[at + 2].audio;
        assert_eq!(third[SAMPLES - CHANNELS..], [0.0, 0.0]);
        assert!(rms(&third[..SAMPLES / 4], 0) > 4.0 * rms(&third[3 * SAMPLES / 4..], 0));
        let back = at + lost;
        assert_eq!(listener.heard[back].audio, faded_in(&replayed[back].frame));
        let stats = listener.stats();
        assert_eq!(
            (stats.concealed, stats.lost, stats.depth_frames),
            (3, lost as u64, 1),
            "{lost} lost"
        );
        listener.assert_no_clicks();
    }
}

// Four frames late within 2 s, 2 percent: the first two frames late, past
// the frame after it, which has already played; the others one frame late,
// each within 5 s of the late frame before it, so none grows the buffer at
// once. The check after them adds one frame.
fn two_percent_late(sent: &[Sent]) -> Vec<Option<u64>> {
    let mut arrive = on_time(sent);
    arrive[100] = Some(sent[100].sent_at + TICK + 1);
    for i in [102, 104, 106] {
        arrive[i] = Some(sent[i].sent_at + 1);
    }
    arrive
}

fn grown_by_two_percent_late(listener: &Listener, sent: &[Sent]) {
    let stats = listener.stats();
    assert_eq!(
        (stats.late, stats.depth_frames, stats.inserted),
        (4, 2, 1),
        "{stats:?}"
    );
    for packet in &sent[110..190] {
        assert_eq!(listener.played_at(packet.seq), Some(packet.sent_at + TICK));
    }
}

// The pull that dropped a frame: the one that played the frame after it.
fn the_drop(listener: &Listener, sent: &[Sent]) -> (usize, usize) {
    let skipped: Vec<usize> = listener
        .skipped(&sent[..sent.len() - 1])
        .into_iter()
        .filter(|&i| i >= 110)
        .collect();
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    let dropped = skipped[0];
    let at = listener.pull_at(listener.played_at(sent[dropped + 1].seq).unwrap());
    (dropped, at)
}

// Grown at 1.1 s, the shrink is due 5 s later, at 6.1 s, and waits for the
// first quiet frame, which comes when the sound stops at 7 s. Dropped and
// blended over 2.5 ms, it is not heard.
#[test]
fn two_percent_late_grows_one_frame_then_5_s_clean_shrinks_it_on_a_quiet_frame() {
    let mut sound = music(7.0);
    sound.extend(silence(3.0));
    let sent = spell(&sound, 0, 0);
    let arrive = two_percent_late(&sent);
    let mut listener = Listener::new();
    listener.run(&sent, &arrive, 2 * SECOND, always);
    grown_by_two_percent_late(&listener, &sent);

    listener.run(&sent, &arrive, 10 * SECOND, always);
    let stats = listener.stats();
    assert_eq!(
        (stats.depth_frames, stats.dropped, stats.late, stats.lost),
        (1, 1, 4, 0),
        "{stats:?}"
    );
    let (dropped, at) = the_drop(&listener, &sent);
    assert!((700..705).contains(&dropped), "dropped frame {dropped}");
    let replayed = replay(&sent, &listener.heard);
    let gone = replayed[at].dropped.as_ref().unwrap();
    assert!(rms(gone, 0) < QUIET_RMS && rms(gone, 1) < QUIET_RMS);
    assert_eq!(
        listener.heard[at].audio,
        blended(gone, &replayed[at].frame, 120)
    );
    for packet in &sent[dropped + 1..sent.len() - 1] {
        assert_eq!(listener.played_at(packet.seq), Some(packet.sent_at));
    }
    listener.assert_in_order();
    listener.assert_no_clicks();
}

// The same with sound that never goes quiet: after 2 s of waiting the
// shrink takes the next frame whatever its level, at 8.1 s, blended over
// 5 ms.
#[test]
fn with_no_quiet_frame_the_shrink_waits_2_s_and_blends_over_5_ms() {
    let sent = spell(&music(10.0), 0, 0);
    let arrive = two_percent_late(&sent);
    let mut listener = Listener::new();
    listener.run(&sent, &arrive, 2 * SECOND, always);
    grown_by_two_percent_late(&listener, &sent);

    listener.run(&sent, &arrive, 10 * SECOND, always);
    let stats = listener.stats();
    assert_eq!(
        (stats.depth_frames, stats.dropped, stats.late, stats.lost),
        (1, 1, 4, 0),
        "{stats:?}"
    );
    let (dropped, at) = the_drop(&listener, &sent);
    assert!((808..812).contains(&dropped), "dropped frame {dropped}");
    let replayed = replay(&sent, &listener.heard);
    let gone = replayed[at].dropped.as_ref().unwrap();
    assert!(rms(gone, 0) > 0.1);
    assert_eq!(
        listener.heard[at].audio,
        blended(gone, &replayed[at].frame, 240)
    );
    for packet in &sent[dropped + 1..sent.len() - 1] {
        assert_eq!(listener.played_at(packet.seq), Some(packet.sent_at));
    }
    listener.assert_in_order();
    listener.assert_no_clicks();
}

// The render thread misses five turns. The play point falls 50 ms behind,
// and since no late frame asked for that depth it comes off at once, at the
// next quiet frames, two in each 50 ms gap.
#[test]
fn a_burst_after_a_render_stall_comes_off_at_once() {
    let sent = spell(&bursts(6.0), 0, 0);
    let stall = 2 * SECOND..2 * SECOND + 5 * TICK;
    let mut listener = Listener::new();
    listener.run(&sent, &on_time(&sent), 6 * SECOND, |now| {
        if stall.contains(&now) {
            Ear::Stalled
        } else {
            Ear::Pulling
        }
    });
    let behind = sent
        .iter()
        .find(|packet| packet.sent_at > stall.end)
        .unwrap();
    assert_eq!(
        listener.played_at(behind.seq),
        Some(behind.sent_at + 5 * TICK)
    );
    let caught_up = 2 * SECOND + 3 * SECOND / 5;
    for packet in &sent[..sent.len() - 1] {
        if packet.sent_at >= caught_up {
            assert_eq!(listener.played_at(packet.seq), Some(packet.sent_at));
        }
    }
    let stats = listener.stats();
    assert_eq!(
        (stats.depth_frames, stats.late, stats.lost, stats.dropped),
        (1, 0, 0, 5),
        "{stats:?}"
    );
    let skipped = listener.skipped(&sent[..sent.len() - 1]);
    assert_eq!(skipped.len(), 5);
    assert!(skipped.iter().all(|&i| in_a_gap(i)), "{skipped:?}");
    listener.assert_in_order();
    listener.assert_no_clicks();
}

// After the frame marked last: one faded frame of concealment, not counted
// as made up, then nothing. The next spell is decoded from a reset decoder.
#[test]
fn last_ends_with_a_fade_and_the_decoder_reset() {
    let mut first = spell(&music(1.0), 1000, 0);
    first.last_mut().unwrap().last = true;
    let second = spell(&music(1.0), 1100, 2 * SECOND);
    let mut sent = first.clone();
    sent.extend(second.iter().cloned());
    let mut listener = Listener::new();
    listener.run(&sent, &on_time(&sent), 3 * SECOND + TICK, always);

    let end = listener.pull_at(first.last().unwrap().sent_at) + 1;
    let start = listener.pull_at(second[0].sent_at);
    let between = listener.sources(end..start);
    assert_eq!(between[0], Some(Source::End));
    assert!(between[1..].iter().all(Option::is_none));
    let faded = &listener.heard[end].audio;
    assert_eq!(faded[SAMPLES - CHANNELS..], [0.0, 0.0]);
    assert!(rms(&faded[..SAMPLES / 4], 0) > 0.1);

    let mut fresh = Decoder::new().unwrap();
    let mut carried_on = Decoder::new().unwrap();
    let mut out = [0f32; SAMPLES];
    for packet in &first {
        carried_on.decode(&packet.frame, &mut out).unwrap();
    }
    carried_on.conceal(&mut out).unwrap();
    for (n, packet) in second.iter().enumerate().take(5) {
        fresh.decode(&packet.frame, &mut out).unwrap();
        let expected = if n == 0 { faded_in(&out) } else { out.to_vec() };
        assert_eq!(listener.heard[start + n].audio, expected, "frame {n}");
        carried_on.decode(&packet.frame, &mut out).unwrap();
        if n == 0 {
            assert_ne!(listener.heard[start].audio, faded_in(&out));
        }
    }
    let stats = listener.stats();
    assert_eq!((stats.concealed, stats.lost, stats.late), (0, 0, 0));
    assert!(stats.playing);
    listener.assert_in_order();
    listener.assert_no_clicks();
}

// An outage with no end mark: concealment, the faded frame, silence until
// the cap has passed the newest frame, then nothing. The sharer's encoder
// carried on, and so did the decoder.
#[test]
fn the_cap_with_nothing_new_goes_idle_without_a_reset() {
    let sent = spell(&music(3.0), 0, 0);
    let mut arrive = on_time(&sent);
    for when in &mut arrive[100..200] {
        *when = None;
    }
    let mut listener = Listener::new();
    listener.run(&sent, &arrive, 3 * SECOND, always);
    let gone = listener.pull_at(sent[100].sent_at);
    let back = listener.pull_at(sent[200].sent_at);
    let between = listener.sources(gone..back);
    assert_eq!(
        between[..6],
        [
            Some(Source::Concealed),
            Some(Source::Concealed),
            Some(Source::Concealed),
            Some(Source::Silence),
            Some(Source::Silence),
            Some(Source::Silence)
        ]
    );
    assert!(between[6..].iter().all(Option::is_none));
    assert_eq!(between.len(), 100);

    let mut kept = Decoder::new().unwrap();
    let mut fresh = Decoder::new().unwrap();
    let mut out = [0f32; SAMPLES];
    for packet in &sent[..100] {
        kept.decode(&packet.frame, &mut out).unwrap();
    }
    for _ in 0..3 {
        kept.conceal(&mut out).unwrap();
    }
    kept.decode(&sent[200].frame, &mut out).unwrap();
    assert_eq!(listener.heard[back].audio, faded_in(&out));
    fresh.decode(&sent[200].frame, &mut out).unwrap();
    assert_ne!(listener.heard[back].audio, faded_in(&out));
    for packet in &sent[200..sent.len() - 1] {
        assert_eq!(listener.played_at(packet.seq), Some(packet.sent_at));
    }
    let stats = listener.stats();
    assert_eq!((stats.concealed, stats.lost, stats.late), (3, 100, 0));
    assert!(stats.playing);
    listener.assert_no_clicks();
}

// Deafened for a second while the frames keep coming, two frames deep from
// a lone late frame before. Nothing that fell out meanwhile counts as late,
// lost or dropped, and playing goes on two deep.
#[test]
fn a_pause_loses_and_drops_nothing() {
    let sent = spell(&music(4.0), 0, 0);
    let mut arrive = on_time(&sent);
    arrive[50] = Some(sent[50].sent_at + 1);
    let deaf = SECOND..2 * SECOND;
    let mut listener = Listener::new();
    listener.run(&sent, &arrive, 4 * SECOND, |now| {
        if deaf.contains(&now) {
            Ear::Deafened
        } else {
            Ear::Pulling
        }
    });
    for packet in &sent[50..sent.len() - 2] {
        if packet.sent_at + TICK < deaf.start || packet.sent_at >= deaf.end - TICK {
            assert_eq!(
                listener.played_at(packet.seq),
                Some(packet.sent_at + TICK),
                "seq {}",
                packet.seq
            );
        }
    }
    let stats = listener.stats();
    assert_eq!(
        (stats.late, stats.lost, stats.dropped, stats.inserted),
        (1, 0, 0, 1),
        "{stats:?}"
    );
    assert_eq!((stats.depth_frames, stats.loss_percent), (2, 0.0));
    // Back from silence, the first frame fades in.
    let back = listener.pull_at(deaf.end);
    let replayed = replay(&sent, &listener.heard);
    assert_eq!(listener.heard[back].audio, faded_in(&replayed[back].frame));
    listener.assert_no_clicks();
}

#[test]
fn duplicates_are_played_once() {
    let sent = spell(&music(1.0), 500, 0);
    // Every frame twice, once on time and once a frame later, after it has
    // played; every tenth a third time, right behind the first.
    let mut twice = sent.clone();
    twice.extend(sent.iter().cloned());
    twice.extend(sent.iter().step_by(10).cloned());
    let mut arrive = on_time(&sent);
    arrive.extend(sent.iter().map(|packet| Some(packet.sent_at + TICK)));
    arrive.extend(sent.iter().step_by(10).map(|packet| Some(packet.sent_at)));
    let mut listener = Listener::new();
    listener.run(&twice, &arrive, SECOND + TICK, always);
    for packet in &sent {
        let times = listener
            .heard
            .iter()
            .filter(|heard| {
                heard.pulled.map(|pulled| pulled.source) == Some(Source::Packet(packet.seq))
            })
            .count();
        assert_eq!(times, 1, "seq {}", packet.seq);
    }
    let stats = listener.stats();
    assert_eq!(
        (stats.late, stats.lost, stats.concealed, stats.depth_frames),
        (0, 0, 0, 1)
    );
}

// A frame of a spell that has ended, after its end, is a straggler. Numbering
// that goes back further than any frame is delayed is the sharer starting
// over, even with its first frame's mark lost: a new spell from a reset
// decoder.
#[test]
fn stragglers_and_numbering_going_back() {
    let mut first = spell(&music(1.0), 30_000, 0);
    first.last_mut().unwrap().last = true;
    let mut straggler = first[90].clone();
    straggler.sent_at = first.last().unwrap().sent_at + 30 * TICK;
    let mut second = spell(&music(1.0), 5, 2 * SECOND);
    second[0].first = false;
    let mut sent = first.clone();
    sent.push(straggler.clone());
    sent.extend(second.iter().cloned());
    let mut listener = Listener::new();
    listener.run(&sent, &on_time(&sent), 3 * SECOND + TICK, always);

    let end = listener.pull_at(first.last().unwrap().sent_at) + 1;
    let start = listener.pull_at(second[0].sent_at);
    let between = listener.sources(end..start);
    assert_eq!(between[0], Some(Source::End));
    assert!(between[1..].iter().all(Option::is_none), "{between:?}");
    for packet in &second {
        assert_eq!(listener.played_at(packet.seq), Some(packet.sent_at));
    }
    let mut fresh = Decoder::new().unwrap();
    let mut out = [0f32; SAMPLES];
    fresh.decode(&second[0].frame, &mut out).unwrap();
    assert_eq!(listener.heard[start].audio, faded_in(&out));
    let stats = listener.stats();
    assert_eq!((stats.late, stats.lost, stats.concealed), (0, 0, 0));
    listener.assert_no_clicks();
}

// A frame marked first starts a spell even when the numbering went back only
// a little, and a frame from before it that comes after it never plays.
#[test]
fn frames_from_before_a_first_are_dropped() {
    let mut first = spell(&music(1.0), 2000, 0);
    first.last_mut().unwrap().last = true;
    let second = spell(&music(1.0), 2050, 2 * SECOND);
    let mut old = first[60].clone();
    old.seq = second[0].seq.wrapping_sub(1);
    old.sent_at = second[3].sent_at;
    let mut sent = first.clone();
    sent.extend(second.iter().cloned());
    sent.push(old.clone());
    let mut listener = Listener::new();
    listener.run(&sent, &on_time(&sent), 3 * SECOND + TICK, always);
    for packet in &second {
        assert_eq!(
            listener.played_from(packet.seq, 2 * SECOND),
            Some(packet.sent_at)
        );
    }
    assert_eq!(listener.played_from(old.seq, 2 * SECOND), None);
    let start = listener.pull_at(second[0].sent_at);
    let mut fresh = Decoder::new().unwrap();
    let mut out = [0f32; SAMPLES];
    fresh.decode(&second[0].frame, &mut out).unwrap();
    assert_eq!(listener.heard[start].audio, faded_in(&out));
    let stats = listener.stats();
    assert_eq!((stats.late, stats.lost, stats.concealed), (0, 0, 0));
    listener.assert_in_order();
    listener.assert_no_clicks();
}

// Three deep, a spell starts two frames behind the first frame in, and a
// frame it overtook that comes before its turn plays in it, instead of a
// stand-in that would leave it held behind the play point.
#[test]
fn a_frame_overtaken_at_the_start_still_plays_in_its_turn() {
    let sent = spell(&music(1.0), 0, 0);
    let mut arrive = on_time(&sent);
    arrive[1] = Some(sent[2].sent_at + 1);
    arrive[0] = None;
    let mut listener = Listener::new();
    listener.buffer.set_period(1056);
    let mut end = TICK;
    while end <= SECOND + 3 * TICK {
        listener.run(&sent, &arrive, end, always);
        assert!(listener.stats().held_frames <= 3, "{:?}", listener.stats());
        end += TICK;
    }
    for packet in &sent[1..] {
        assert_eq!(
            listener.played_at(packet.seq),
            Some(packet.sent_at + 2 * TICK)
        );
    }
    let stats = listener.stats();
    assert_eq!((stats.depth_frames, stats.late, stats.lost), (3, 0, 0));
}

// The sharer's sound stops and starts again so fast that the end mark is
// lost and the next spell's first frame comes while sound still plays. The
// decoder starts over for it, and the sound before is carried on by
// concealment and blended out over 2.5 ms rather than cut off.
#[test]
fn a_first_frame_while_sound_plays_starts_the_decoder_over() {
    let first = spell(&music(1.0), 0, 0);
    let second = spell(&music(1.0), 100, SECOND);
    let mut sent = first.clone();
    sent.extend(second.iter().cloned());
    let mut listener = Listener::new();
    listener.run(&sent, &on_time(&sent), 2 * SECOND, always);
    for packet in &sent[..sent.len() - 1] {
        assert_eq!(listener.played_at(packet.seq), Some(packet.sent_at));
    }
    let mut old = Decoder::new().unwrap();
    let mut carried_on = [0f32; SAMPLES];
    for packet in &first {
        old.decode(&packet.frame, &mut carried_on).unwrap();
    }
    old.conceal(&mut carried_on).unwrap();
    let mut fresh = Decoder::new().unwrap();
    let mut started = [0f32; SAMPLES];
    fresh.decode(&second[0].frame, &mut started).unwrap();
    let at = listener.pull_at(second[0].sent_at);
    assert_eq!(
        listener.heard[at].audio,
        blended(&carried_on, &started, 120)
    );
    for (n, packet) in second.iter().enumerate().take(5).skip(1) {
        fresh.decode(&packet.frame, &mut started).unwrap();
        assert_eq!(listener.heard[at + n].audio, started, "frame {n}");
    }
    let stats = listener.stats();
    assert_eq!((stats.late, stats.lost, stats.concealed), (0, 0, 0));
    listener.assert_no_clicks();
}

// A spell starts at the depth the buffer learned. A shrink that came due
// and found no quiet frame before its spell ended happens at the next
// spell's start instead, in the gap between them.
#[test]
fn a_later_spell_starts_at_the_depth_learned_and_takes_a_due_shrink() {
    let mut first = spell(&music(3.0), 0, 0);
    first.last_mut().unwrap().last = true;
    // Past the 5 s a shrink waits, and over before the 2 s it then waits for
    // a quiet frame.
    let mut second = spell(&music(6.0), 300, 4 * SECOND);
    second.last_mut().unwrap().last = true;
    let third = spell(&music(1.0), 900, 11 * SECOND);
    let mut sent = first.clone();
    sent.extend(second.iter().cloned());
    sent.extend(third.iter().cloned());
    let mut arrive = on_time(&sent);
    // A lone late frame: two frames deep from 0.5 s.
    arrive[50] = Some(sent[50].sent_at + 1);
    let mut listener = Listener::new();
    listener.run(&sent, &arrive, 12 * SECOND + TICK, always);
    for packet in &second {
        assert_eq!(listener.played_at(packet.seq), Some(packet.sent_at + TICK));
    }
    for packet in &third {
        assert_eq!(listener.played_at(packet.seq), Some(packet.sent_at));
    }
    let stats = listener.stats();
    assert_eq!(
        (stats.depth_frames, stats.inserted, stats.dropped),
        (1, 1, 1),
        "{stats:?}"
    );
    assert_eq!((stats.late, stats.lost), (1, 0));
    listener.assert_in_order();
    listener.assert_no_clicks();
}

// 500 Hz on the left and 700 Hz on the right, through a lost frame and a
// frame added for a late one.
#[test]
fn left_and_right_stay_apart() {
    let sent = spell(&tones(2.0, 500.0, 700.0), 0, 0);
    let mut arrive = on_time(&sent);
    arrive[60] = None;
    arrive[120] = Some(sent[120].sent_at + 1);
    let mut listener = Listener::new();
    listener.run(&sent, &arrive, 2 * SECOND, always);
    let stats = listener.stats();
    assert_eq!((stats.concealed, stats.inserted), (2, 1), "{stats:?}");
    let played: Vec<f32> = listener.heard[listener.pull_at(sent[10].sent_at)..]
        .iter()
        .flat_map(|heard| heard.audio.iter().copied())
        .collect();
    let (left_own, left_other) = (level_db(&played, 0, 500.0), level_db(&played, 0, 700.0));
    let (right_own, right_other) = (level_db(&played, 1, 700.0), level_db(&played, 1, 500.0));
    println!(
        "left: 500 Hz {left_own:.1} dB, 700 Hz {left_other:.1} dB; right: 700 Hz {right_own:.1} dB, 500 Hz {right_other:.1} dB"
    );
    assert!(left_own - left_other > 40.0 && right_own - right_other > 40.0);
    assert!((left_own + 10.5).abs() < 1.5 && (right_own + 10.5).abs() < 1.5);
    listener.assert_no_clicks();
}

// The sharer's sound card 300 ppm slow and 300 ppm fast against the
// listener's: each frame comes 10 ms x (1 +- 300e-6) after the one before,
// while the listener pulls every 10 ms. Ten minutes is 180 ms of drift, 18
// frames, and each should cost one frame added or dropped and no more, an
// added one a single stand-in, with the buffer never deeper than its minimum
// plus two.
#[test]
fn clock_drift_costs_one_frame_per_frame_of_drift() {
    // A second of the tones repeats exactly, 440 and 660 whole cycles. Its
    // frames from an encoder that has already seen one such second are sent
    // over and over.
    let mut encoder = Encoder::new().unwrap();
    let mut out = [0u8; MAX_PACKET];
    let sound = music(2.0);
    let (frames, _) = sound.as_chunks::<SAMPLES>();
    let pool: Vec<Vec<u8>> = frames
        .iter()
        .map(|pcm| {
            let len = encoder.encode(pcm, &mut out).unwrap();
            out[..len].to_vec()
        })
        .skip(100)
        .collect();
    assert_eq!(pool.len(), 100);
    let pulls = 600 * 100;
    for ppm in [300.0, -300.0] {
        let spacing = TICK as f64 * (1.0 + ppm * 1e-6);
        let mut buffer = Buffer::new().unwrap();
        let mut frame = [0f32; SAMPLES];
        let (mut next, mut stand_ins, mut deepest) = (0u64, 0, 0);
        for pull in 0..pulls {
            // The first frame lands 3.3 ms into a pull.
            while 158.0 + next as f64 * spacing <= (pull * TICK) as f64 {
                let packet = SoundPacket {
                    seq: next as u16,
                    frame: &pool[(next % 100) as usize],
                    first: next == 0,
                    last: false,
                };
                buffer.push(packet).unwrap();
                next += 1;
            }
            if let Some(pulled) = buffer.pull(&mut frame)
                && matches!(pulled.source, Source::Concealed | Source::Silence)
            {
                stand_ins += 1;
            }
            deepest = deepest.max(buffer.stats().depth_frames);
        }
        let stats = buffer.stats();
        println!(
            "{ppm:+} ppm for ten minutes: {} inserted, {} dropped, {stand_ins} stand-ins, {} late, never deeper than {deepest} frames",
            stats.inserted, stats.dropped, stats.late
        );
        let changes = stats.inserted + stats.dropped;
        assert!((17..=19).contains(&changes), "{stats:?}");
        if ppm > 0.0 {
            assert_eq!(stats.dropped, 0, "{stats:?}");
            assert!(stand_ins <= stats.inserted, "{stand_ins} stand-ins");
            assert_eq!(stats.late, stats.inserted, "{stats:?}");
        } else {
            assert_eq!((stats.inserted, stand_ins, stats.late), (0, 0, 0));
        }
        assert_eq!(stats.lost, 0, "{stats:?}");
        assert!(deepest <= 3, "{deepest} frames deep");
        assert!(stats.depth_ms <= MAX_DEPTH_MS);
    }
}
