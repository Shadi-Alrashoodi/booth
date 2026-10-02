use std::f32::consts::TAU;

use voice::codec::{Encoder, MAX_FRAME, MAX_PACKET, Mode, SAMPLE_RATE};
use voice::jitter::{JitterBuffer, JitterStats, MAX_DEPTH_MS, Pulled, Source, VoicePacket};

// No step between two output samples may be larger than this, joins included:
// concealment, the fade to silence, the fade back in, dropped and inserted
// frames, restarts. The test tone is 330 Hz at 0.4 of full scale, whose own
// largest step is 0.017, and 0.018 after the codec; the worst join in these
// tests measured 0.026. Without the fade in at a restart it was 0.34.
const MAX_JOIN_STEP: f32 = 0.04;

// 5 ms at 48 kHz, the unit the listener below works in.
const TICK: u64 = 240;
const SECOND: u64 = SAMPLE_RATE as u64;

#[derive(Clone)]
struct Sent {
    seq: u16,
    frame: Vec<u8>,
    previous: Option<Vec<u8>>,
    redundancy: bool,
    last: bool,
    // When the frame was complete and left the talker, in samples.
    sent_at: u64,
}

impl Sent {
    fn packet(&self) -> VoicePacket<'_> {
        VoicePacket {
            seq: self.seq,
            frame: &self.frame,
            previous: self.previous.as_deref(),
            redundancy: self.redundancy,
            last: self.last,
        }
    }
}

fn tone(seconds: f32) -> Vec<f32> {
    let samples = (seconds * SAMPLE_RATE as f32) as usize;
    (0..samples)
        .map(|i| 0.4 * (TAU * 330.0 * i as f32 / SAMPLE_RATE as f32).sin())
        .collect()
}

fn silence(seconds: f32) -> Vec<f32> {
    vec![0.0; (seconds * SAMPLE_RATE as f32) as usize]
}

// A syllable every 200 ms: 150 ms of a 220 Hz tone at 0.3, then 50 ms of
// the microphone's own noise, which runs underneath throughout at
// `noise_db` dBFS. The syllables start and stop on zero crossings. The
// noise is low-passed, as fan and room noise mostly is, so its own steps
// stay well under MAX_JOIN_STEP.
const SYLLABLE: u64 = SECOND / 5;
const VOICED: u64 = SYLLABLE * 3 / 4;

fn speech(seconds: f32, noise_db: f32) -> Vec<f32> {
    let samples = (seconds * SAMPLE_RATE as f32) as u64;
    let mut state = 0x853c_49e6_748f_ea9bu64;
    let mut smooth = 0.0f32;
    let mut noise: Vec<f32> = (0..samples)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let white = (state >> 40) as f32 / (1u64 << 23) as f32 - 1.0;
            smooth = 0.9 * smooth + 0.1 * white;
            smooth
        })
        .collect();
    let rms = (noise.iter().map(|n| n * n).sum::<f32>() / noise.len() as f32).sqrt();
    let scale = 10f32.powf(noise_db / 20.0) / rms;
    for (i, sample) in noise.iter_mut().enumerate() {
        *sample *= scale;
        if (i as u64) % SYLLABLE < VOICED {
            let t = i as f32 / SAMPLE_RATE as f32;
            *sample += 0.3 * (TAU * 220.0 * t).sin();
        }
    }
    noise
}

// Whether the frame that ends at `sent_at` was all microphone noise.
fn in_a_pause(sent_at: u64) -> bool {
    (sent_at - TICK) % SYLLABLE >= VOICED
}

// One talker's transmission: `segments` played in order through one encoder,
// switching mode between them, numbered on from `first_seq`, starting at
// `start` samples.
fn talk(segments: &[(Mode, &[f32])], first_seq: u16, start: u64, redundancy: bool) -> Vec<Sent> {
    let mut encoder = Encoder::new(segments[0].0, true).unwrap();
    let mut out = [0u8; MAX_PACKET];
    let mut sent: Vec<Sent> = Vec::new();
    let mut clock = start;
    for &(mode, signal) in segments {
        encoder.set_mode(mode).unwrap();
        for frame in signal.chunks_exact(mode.frame_samples()) {
            let len = encoder.encode(frame, &mut out).unwrap();
            clock += frame.len() as u64;
            let previous = match sent.last() {
                Some(before) if redundancy => Some(before.frame.clone()),
                _ => None,
            };
            sent.push(Sent {
                seq: first_seq.wrapping_add(sent.len() as u16),
                frame: out[..len].to_vec(),
                previous,
                redundancy,
                last: false,
                sent_at: clock,
            });
        }
    }
    sent
}

fn low_delay(signal: &[f32], first_seq: u16) -> Vec<Sent> {
    talk(&[(Mode::LowDelay, signal)], first_seq, 0, false)
}

fn on_time(sent: &[Sent]) -> Vec<Option<u64>> {
    sent.iter().map(|packet| Some(packet.sent_at)).collect()
}

// What came out of one pull, and when. Ticks where the buffer had nothing are
// kept as silence, since that is what the mixer plays for this talker.
struct Heard {
    at: u64,
    pulled: Option<Pulled>,
    audio: Vec<f32>,
}

// The render side of one talker: pulls a frame whenever the last one has run
// out, and every 5 ms while the buffer is idle. Before each pull it pushes
// every packet that has arrived by then, oldest arrival first.
struct Listener {
    buffer: JitterBuffer,
    now: u64,
    pushed_through: Option<u64>,
    heard: Vec<Heard>,
}

impl Listener {
    fn new() -> Listener {
        Listener {
            buffer: JitterBuffer::new().unwrap(),
            now: 0,
            pushed_through: None,
            heard: Vec::new(),
        }
    }

    fn run(
        &mut self,
        sent: &[Sent],
        arrive: &[Option<u64>],
        until: u64,
        pulling: impl Fn(u64) -> bool,
    ) {
        let mut frame = [0f32; MAX_FRAME];
        while self.now < until {
            let mut due: Vec<(u64, usize)> = arrive
                .iter()
                .enumerate()
                .filter_map(|(i, at)| {
                    let at = (*at)?;
                    let fresh = self.pushed_through.is_none_or(|done| at > done);
                    (fresh && at <= self.now).then_some((at, i))
                })
                .collect();
            due.sort();
            for (_, i) in due {
                self.buffer.push(sent[i].packet()).unwrap();
            }
            self.pushed_through = Some(self.now);
            if !pulling(self.now) {
                self.now += TICK;
                continue;
            }
            let pulled = self.buffer.pull(&mut frame);
            let audio = match pulled {
                Some(pulled) => frame[..pulled.samples].to_vec(),
                None => vec![0.0; TICK as usize],
            };
            let at = self.now;
            self.now += audio.len() as u64;
            self.heard.push(Heard { at, pulled, audio });
        }
    }

    fn stats(&self) -> JitterStats {
        self.buffer.stats()
    }

    fn played_at(&self, seq: u16) -> Option<u64> {
        self.heard
            .iter()
            .find_map(|heard| match heard.pulled?.source {
                Source::Packet(s) | Source::Copy(s) | Source::Repaired(s) if s == seq => {
                    Some(heard.at)
                }
                _ => None,
            })
    }

    fn sources(&self) -> Vec<Source> {
        self.heard
            .iter()
            .filter_map(|heard| heard.pulled.map(|pulled| pulled.source))
            .collect()
    }

    // The largest step between neighbouring samples wherever the output was
    // continuous (a tick without a pull, as when deafened, breaks it).
    fn largest_step(&self) -> (f32, u64) {
        let mut largest = (0.0f32, 0);
        for (i, heard) in self.heard.iter().enumerate() {
            let before = i
                .checked_sub(1)
                .map(|j| &self.heard[j])
                .filter(|prev| prev.at + prev.audio.len() as u64 == heard.at)
                .and_then(|prev| prev.audio.last().copied());
            let mut last = before;
            for (k, &sample) in heard.audio.iter().enumerate() {
                if let Some(last) = last
                    && (sample - last).abs() > largest.0
                {
                    largest = ((sample - last).abs(), heard.at + k as u64);
                }
                last = Some(sample);
            }
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

    // Every frame that came from the talker came after the one before it.
    fn assert_in_order(&self) {
        let mut before: Option<u16> = None;
        for heard in &self.heard {
            let seq = match heard.pulled.map(|pulled| pulled.source) {
                Some(Source::Packet(s) | Source::Copy(s) | Source::Repaired(s)) => s,
                _ => continue,
            };
            if let Some(before) = before {
                let step = seq.wrapping_sub(before) as i16;
                assert!(
                    step > 0,
                    "seq {seq} played after {before} at {:.1} ms",
                    heard.at as f64 / 48.0
                );
            }
            before = Some(seq);
        }
    }
}

fn always(_: u64) -> bool {
    true
}

#[test]
fn starts_at_one_frame() {
    let sent = low_delay(&tone(1.0), 100);
    let mut listener = Listener::new();
    listener.run(&sent, &on_time(&sent), SECOND + TICK, always);
    // Each frame plays at the first pull after it arrives: one frame of
    // buffer at most.
    for packet in &sent {
        assert_eq!(
            listener.played_at(packet.seq),
            Some(packet.sent_at),
            "seq {}",
            packet.seq
        );
    }
    let stats = listener.stats();
    assert_eq!(
        (stats.depth_frames, stats.depth_ms, stats.frame_ms),
        (1, 5, 5)
    );
    assert_eq!((stats.late, stats.lost, stats.concealed), (0, 0, 0));
    assert_eq!((stats.inserted, stats.dropped), (0, 0));
    listener.assert_no_clicks();
}

#[test]
fn starts_at_two_frames_with_redundancy() {
    let sent = talk(&[(Mode::LowDelay, &tone(1.0))], 9, 0, true);
    assert!(sent[0].previous.is_none());
    let mut listener = Listener::new();
    listener.run(&sent, &on_time(&sent), SECOND + 2 * TICK, always);
    for packet in &sent {
        assert_eq!(listener.played_at(packet.seq), Some(packet.sent_at + TICK));
    }
    let stats = listener.stats();
    assert_eq!((stats.depth_frames, stats.depth_ms), (2, 10));
    assert_eq!(
        (stats.inserted, stats.concealed, stats.redundancy_used),
        (0, 0, 0)
    );
    listener.assert_no_clicks();
}

#[test]
fn grows_past_one_percent_late() {
    let sent = low_delay(&tone(6.0), 0);

    // Four late packets within 2 s are exactly 1 percent: no change.
    let mut arrive = on_time(&sent);
    for i in [400, 500, 600, 700] {
        arrive[i] = Some(sent[i].sent_at + TICK);
    }
    arrive[760] = Some(sent[760].sent_at + TICK);
    let mut listener = Listener::new();
    listener.run(&sent, &arrive, sent[759].sent_at, always);
    let stats = listener.stats();
    assert_eq!((stats.late, stats.depth_frames, stats.inserted), (4, 1, 0));

    // A fifth one is more than 1 percent, and the next check grows the buffer.
    listener.run(&sent, &arrive, sent[760].sent_at + 25 * TICK, always);
    let stats = listener.stats();
    assert_eq!((stats.late, stats.depth_frames, stats.inserted), (5, 2, 1));
    listener.run(&sent, &arrive, sent[1199].sent_at + TICK + 1, always);
    for packet in &sent[800..] {
        assert_eq!(listener.played_at(packet.seq), Some(packet.sent_at + TICK));
    }
    assert_eq!(listener.stats().depth_frames, 2);
    listener.assert_no_clicks();

    // Late packets more than 2 s apart never add up to more than 1 percent.
    let mut arrive = on_time(&sent);
    for i in [100, 150, 200, 250, 700, 750, 800, 850] {
        arrive[i] = Some(sent[i].sent_at + TICK);
    }
    let mut listener = Listener::new();
    listener.run(&sent, &arrive, sent[1199].sent_at + TICK, always);
    let stats = listener.stats();
    assert_eq!((stats.late, stats.depth_frames, stats.inserted), (8, 1, 0));
}

#[test]
fn never_deeper_than_60_ms() {
    let sent = low_delay(&tone(10.0), 60_000);
    // Up to 120 ms of jitter: twice the cap.
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let arrive: Vec<Option<u64>> = sent
        .iter()
        .map(|packet| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            Some(packet.sent_at + (state % 25) * TICK)
        })
        .collect();
    let mut listener = Listener::new();
    let mut deepest = 0;
    let mut end = TICK;
    while end < 11 * SECOND {
        listener.run(&sent, &arrive, end, always);
        let stats = listener.stats();
        assert!(stats.depth_ms <= MAX_DEPTH_MS, "{stats:?}");
        assert!(
            stats.held_frames * stats.frame_ms <= MAX_DEPTH_MS,
            "{stats:?}"
        );
        deepest = deepest.max(stats.depth_frames);
        end += TICK;
    }
    let stats = listener.stats();
    assert_eq!(deepest, 12);
    // What came later than the cap was thrown away and counted as lost.
    assert!(stats.lost > 0 && stats.late > 0, "{stats:?}");
}

#[test]
fn shrinks_after_5s_clean_only_on_a_quiet_frame() {
    // Loud, a quiet stretch too soon after growing, loud past the 5 s mark,
    // then quiet.
    let mut signal = tone(1.5);
    signal.extend(silence(1.5));
    signal.extend(tone(6.0));
    signal.extend(silence(3.0));
    let sent = low_delay(&signal, 0);
    let mut arrive = on_time(&sent);
    for i in [150, 160, 170, 180, 190] {
        arrive[i] = Some(sent[i].sent_at + TICK);
    }
    let mut listener = Listener::new();
    listener.run(&sent, &arrive, 2 * SECOND, always);
    assert_eq!(listener.stats().depth_frames, 2);

    listener.run(&sent, &arrive, 9 * SECOND, always);
    let stats = listener.stats();
    assert_eq!((stats.depth_frames, stats.dropped), (2, 0));

    listener.run(&sent, &arrive, 12 * SECOND, always);
    let stats = listener.stats();
    assert_eq!((stats.depth_frames, stats.dropped), (1, 1));
    // Played one frame of buffer later than on arrival before the drop, on
    // arrival after it; the drop fell in the quiet stretch.
    let dropped_at = sent[200..]
        .iter()
        .find(|packet| listener.played_at(packet.seq) == Some(packet.sent_at))
        .map(|packet| packet.sent_at)
        .unwrap();
    assert!(dropped_at > 9 * SECOND, "dropped at {dropped_at}");
    listener.assert_no_clicks();
}

#[test]
fn depth_is_kept_across_transmissions() {
    let signal = tone(3.0);
    let mut first = low_delay(&signal, 1000);
    first.last_mut().unwrap().last = true;
    let second = talk(
        &[(Mode::LowDelay, &signal[..SECOND as usize])],
        1600,
        4 * SECOND,
        false,
    );
    let mut sent = first.clone();
    sent.extend(second.iter().cloned());

    // Six packets two frames late: the buffer grows to three frames.
    let mut arrive = on_time(&sent);
    for i in [100, 110, 120, 130, 140, 150] {
        arrive[i] = Some(sent[i].sent_at + 2 * TICK);
    }
    let mut listener = Listener::new();
    listener.run(&sent, &arrive, 3 * SECOND + 3 * TICK, always);
    let stats = listener.stats();
    assert_eq!(stats.depth_frames, 3);
    let concealed = stats.concealed;

    // The talker let go: one faded frame, then nothing, and nothing counted
    // as concealed.
    listener.run(&sent, &arrive, 4 * SECOND, always);
    let tail: Vec<Source> = listener
        .heard
        .iter()
        .filter(|heard| heard.at > first.last().unwrap().sent_at + 2 * TICK)
        .filter_map(|heard| heard.pulled.map(|pulled| pulled.source))
        .collect();
    assert_eq!(tail, [Source::Silence]);
    let stats = listener.stats();
    assert!(!stats.playing);
    assert_eq!(stats.concealed, concealed);

    // The next sentence starts three frames deep: two frames after the first
    // packet arrives.
    listener.run(&sent, &arrive, 5 * SECOND + 3 * TICK, always);
    for packet in &second {
        assert_eq!(
            listener.played_at(packet.seq),
            Some(packet.sent_at + 2 * TICK)
        );
    }
    let stats = listener.stats();
    assert_eq!((stats.depth_frames, stats.lost), (3, 0));
    listener.assert_no_clicks();
}

// What voice is held to: a 200 ms outage produces a gap and then normal audio
// at one frame of buffer again, with no click and no lasting delay.
#[test]
fn outage_of_200_ms_then_one_frame_of_buffer_again() {
    let sent = low_delay(&tone(3.0), 30);
    let outage = 200..240;
    let resumed = &sent[outage.end];

    // Packets lost on the way, and packets held up and delivered in one burst.
    let mut dropped = on_time(&sent);
    let mut held_up = on_time(&sent);
    for i in outage.clone() {
        dropped[i] = None;
        held_up[i] = Some(resumed.sent_at);
    }

    for arrive in [dropped, held_up] {
        let mut listener = Listener::new();
        listener.run(&sent, &arrive, 3 * SECOND + TICK, always);

        let during: Vec<Source> = listener
            .heard
            .iter()
            .filter(|heard| heard.at > sent[outage.start - 1].sent_at && heard.at < resumed.sent_at)
            .filter_map(|heard| heard.pulled.map(|pulled| pulled.source))
            .collect();
        assert_eq!(during[..5], [Source::Concealed; 5]);
        assert!(during[5..].iter().all(|&source| source == Source::Silence));
        for packet in &sent[outage.clone()] {
            assert_eq!(listener.played_at(packet.seq), None);
        }
        for packet in &sent[outage.end..] {
            assert_eq!(listener.played_at(packet.seq), Some(packet.sent_at));
        }
        let stats = listener.stats();
        assert_eq!((stats.depth_frames, stats.late, stats.inserted), (1, 0, 0));
        assert_eq!((stats.lost, stats.concealed), (40, 5));
        assert!((9.0..=11.0).contains(&stats.loss_percent), "{stats:?}");
        listener.assert_no_clicks();
    }
}

#[test]
fn short_loss_is_concealed_and_joins_back_smoothly() {
    let sent = low_delay(&tone(1.0), 0);
    let mut arrive = on_time(&sent);
    for i in [50, 100, 101, 150, 151, 152, 153] {
        arrive[i] = None;
    }
    let mut listener = Listener::new();
    listener.run(&sent, &arrive, SECOND + TICK, always);
    let stats = listener.stats();
    assert_eq!((stats.concealed, stats.lost, stats.depth_frames), (7, 7, 1));
    assert_eq!(listener.played_at(sent[154].seq), Some(sent[154].sent_at));
    listener.assert_no_clicks();
}

// The repair copy and the 10 ms mode follow loss one or two frames in a row
// only, since a longer run is an outage neither can repair.
#[test]
fn loss_one_or_two_in_a_row_is_counted_apart_from_outages() {
    let sent = low_delay(&tone(4.0), 65_300);
    let mut arrive = on_time(&sent);
    let singles_and_pairs = [250, 300, 350, 351, 560, 720];
    let runs = (150..190).chain(400..403).chain(450..460);
    for i in runs.chain(singles_and_pairs) {
        arrive[i] = None;
    }
    let mut listener = Listener::new();

    // The last 2 s judged, 400 frames, end a little behind the play point,
    // and start inside the 40-frame outage: its tail is in, and still not
    // scattered.
    listener.run(&sent, &arrive, 3 * SECOND, always);
    let stats = listener.stats();
    let percent = |frames: u32| frames as f32 * 100.0 / 400.0;
    assert_eq!(stats.scattered_percent, percent(5), "{stats:?}");
    let outage_tail = (1..=4).map(|tail| percent(tail + 5 + 3 + 10));
    assert!(
        outage_tail.into_iter().any(|all| all == stats.loss_percent),
        "{stats:?}"
    );

    // A single loss is loss as soon as it is judged, and scattered only
    // once the frame after it is in: until then it could be the start of
    // an outage.
    while listener.stats().lost < 59 {
        let next = listener.now + TICK;
        listener.run(&sent, &arrive, next, always);
    }
    assert_eq!(listener.stats().scattered_percent, percent(3));
    let next = listener.now + TICK;
    listener.run(&sent, &arrive, next, always);
    assert_eq!(listener.stats().scattered_percent, percent(4));

    listener.run(&sent, &arrive, 4 * SECOND, always);
    let stats = listener.stats();
    assert_eq!(stats.scattered_percent, percent(2), "{stats:?}");
    assert_eq!(stats.loss_percent, percent(3 + 10 + 2), "{stats:?}");
    listener.assert_no_clicks();
}

#[test]
fn sequence_numbers_wrap() {
    let sent = low_delay(&tone(2.0), 65_400);
    let mut listener = Listener::new();
    listener.run(&sent, &on_time(&sent), 2 * SECOND + TICK, always);
    assert!(sent.iter().any(|packet| packet.seq == 0));
    for packet in &sent {
        assert_eq!(listener.played_at(packet.seq), Some(packet.sent_at));
    }
    let stats = listener.stats();
    assert_eq!(
        (stats.lost, stats.late, stats.concealed, stats.depth_frames),
        (0, 0, 0, 1)
    );
}

#[test]
fn duplicates_are_played_once() {
    let sent = low_delay(&tone(1.0), 500);
    // Every packet twice: once on time and once a frame later, after it has
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
            .sources()
            .iter()
            .filter(|&&source| source == Source::Packet(packet.seq))
            .count();
        assert_eq!(times, 1, "seq {}", packet.seq);
    }
    let stats = listener.stats();
    assert_eq!(
        (stats.late, stats.lost, stats.concealed, stats.depth_frames),
        (0, 0, 0, 1)
    );
}

#[test]
fn packets_reordered_within_the_buffer_play_in_order() {
    // Two frames deep with redundancy on; every pair arrives swapped, the
    // second frame first.
    let sent = talk(&[(Mode::LowDelay, &tone(1.0))], 0, 0, true);
    let mut swapped = Vec::new();
    for pair in sent.chunks(2) {
        swapped.extend(pair.iter().rev().cloned());
    }
    let arrive: Vec<Option<u64>> = swapped
        .iter()
        .map(|packet| Some(packet.sent_at.div_ceil(2 * TICK) * 2 * TICK))
        .collect();
    let mut listener = Listener::new();
    listener.run(&swapped, &arrive, SECOND + 2 * TICK, always);
    for packet in &sent {
        let at = packet.sent_at.div_ceil(2 * TICK) * 2 * TICK;
        let played = listener
            .heard
            .iter()
            .find(|heard| heard.pulled.map(|p| p.source) == Some(Source::Packet(packet.seq)));
        assert!(
            played.is_some(),
            "seq {} not played from its own packet",
            packet.seq
        );
        assert!(listener.played_at(packet.seq).unwrap() >= at);
    }
    listener.assert_in_order();
    let stats = listener.stats();
    assert_eq!((stats.late, stats.concealed, stats.lost), (0, 0, 0));
    listener.assert_no_clicks();
}

#[test]
fn a_copy_fills_a_single_loss() {
    let sent = talk(&[(Mode::LowDelay, &tone(1.0))], 0, 0, true);
    let mut arrive = on_time(&sent);
    arrive[50] = None;
    let mut listener = Listener::new();
    listener.run(&sent, &arrive, SECOND + 2 * TICK, always);
    let played = listener
        .heard
        .iter()
        .find(|heard| heard.at == sent[50].sent_at + TICK)
        .and_then(|heard| heard.pulled)
        .map(|pulled| pulled.source);
    assert_eq!(played, Some(Source::Copy(sent[50].seq)));
    let stats = listener.stats();
    assert_eq!(
        (stats.redundancy_used, stats.concealed, stats.lost),
        (1, 0, 1)
    );
    assert!(stats.loss_percent > 0.0);
    listener.assert_no_clicks();
}

#[test]
fn repair_data_fills_a_single_loss_in_the_10_ms_mode() {
    let sent = talk(&[(Mode::Repair, &tone(2.0))], 0, 0, false);
    let mut arrive = on_time(&sent);
    arrive[60] = None;
    let mut listener = Listener::new();
    listener.run(&sent, &arrive, 2 * SECOND + 4 * TICK, always);
    assert!(listener.sources().contains(&Source::Repaired(sent[60].seq)));
    let stats = listener.stats();
    assert_eq!(
        (stats.frame_ms, stats.depth_frames, stats.depth_ms),
        (10, 2, 20)
    );
    assert_eq!((stats.repaired, stats.concealed, stats.lost), (1, 0, 1));
    listener.assert_no_clicks();
}

#[test]
fn a_mode_switch_mid_sentence_keeps_the_audio_going() {
    let signal = tone(2.0);
    let (low, repair) = signal.split_at(SECOND as usize);
    let sent = talk(&[(Mode::LowDelay, low), (Mode::Repair, repair)], 0, 0, true);
    let mut listener = Listener::new();
    listener.run(&sent, &on_time(&sent), 2 * SECOND + 6 * TICK, always);

    let first = listener.played_at(sent[0].seq).unwrap();
    let last = listener.played_at(sent.last().unwrap().seq).unwrap();
    let between: Vec<Source> = listener
        .heard
        .iter()
        .filter(|heard| heard.at >= first && heard.at <= last)
        .filter_map(|heard| heard.pulled.map(|pulled| pulled.source))
        .collect();
    assert!(!between.contains(&Source::Silence));
    let made_up = between.iter().filter(|&&s| s == Source::Concealed).count();
    assert!(made_up <= 2, "{made_up} concealed frames at the switch");
    let stats = listener.stats();
    assert_eq!(stats.frame_ms, 10);
    assert!(stats.depth_frames >= 2, "{stats:?}");
    assert_eq!(stats.lost, 0);
    listener.assert_no_clicks();
}

#[test]
fn without_an_end_mark_the_tail_is_concealed_then_idle() {
    let sent = low_delay(&tone(0.5), 0);
    let mut listener = Listener::new();
    listener.run(&sent, &on_time(&sent), SECOND, always);
    let after: Vec<Option<Source>> = listener
        .heard
        .iter()
        .filter(|heard| heard.at > sent.last().unwrap().sent_at)
        .map(|heard| heard.pulled.map(|pulled| pulled.source))
        .collect();
    // Concealment, a faded frame, silence to the cap, then nothing. Only the
    // frames Opus made up count as concealed.
    assert_eq!(after[..5], [Some(Source::Concealed); 5]);
    assert_eq!(after[5..12], [Some(Source::Silence); 7]);
    assert!(after[12..].iter().all(Option::is_none));
    let stats = listener.stats();
    assert_eq!((stats.concealed, stats.lost, stats.playing), (5, 0, false));
    listener.assert_no_clicks();
}

#[test]
fn after_pulls_stop_it_restarts_at_its_depth() {
    // As when deafened: a second with no pulls while the packets keep coming.
    let sent = low_delay(&tone(4.0), 0);
    let deaf = SECOND..2 * SECOND;
    let mut listener = Listener::new();
    listener.run(&sent, &on_time(&sent), 4 * SECOND + TICK, |now| {
        !deaf.contains(&now)
    });
    for packet in sent.iter().filter(|packet| packet.sent_at >= deaf.end) {
        assert_eq!(listener.played_at(packet.seq), Some(packet.sent_at));
    }
    let stats = listener.stats();
    assert_eq!((stats.depth_frames, stats.late, stats.lost), (1, 0, 0));
    assert!(stats.dropped >= 190, "{stats:?}");
    listener.assert_no_clicks();
}

// The first 10 ms frame is complete 5 ms after a 5 ms frame would have been,
// so at one frame of buffer its number goes by as a 5 ms stand-in before it
// arrives. Arriving on time, a little late or not at all, playing still
// switches to it and carries on at the 10 ms mode's depth.
#[test]
fn first_10_ms_frame_after_its_number_went_by() {
    let signal = tone(2.0);
    let (low, repair) = signal.split_at(SECOND as usize);
    let first = 200;
    for redundancy in [false, true] {
        let sent = talk(
            &[(Mode::LowDelay, low), (Mode::Repair, repair)],
            0,
            0,
            redundancy,
        );
        assert_eq!(sent[first].frame.len(), Mode::Repair.packet_bytes());
        for delay in [Some(0), Some(48), None] {
            let case = format!("redundancy {redundancy}, first 10 ms frame delayed {delay:?}");
            let mut arrive = on_time(&sent);
            arrive[first] = delay.map(|delay| sent[first].sent_at + delay);
            let mut listener = Listener::new();
            listener.run(&sent, &arrive, 2 * SECOND + 8 * TICK, always);

            // Every later frame plays from its own packet, within the 10 ms
            // mode's extra frame and the 5 ms the switch shifted by.
            for packet in &sent[first + 1..] {
                let at = listener.played_at(packet.seq);
                assert!(
                    at.is_some_and(|at| at <= packet.sent_at + 3 * TICK),
                    "{case}: seq {} sent at {} played at {at:?}",
                    packet.seq,
                    packet.sent_at
                );
            }
            let seq = sent[first].seq;
            let filled = listener.sources().into_iter().find(|source| {
                matches!(source, Source::Packet(s) | Source::Copy(s) | Source::Repaired(s) if *s == seq)
            });
            let expected = match (delay, redundancy) {
                (Some(_), _) => Source::Packet(seq),
                (None, true) => Source::Copy(seq),
                (None, false) => Source::Repaired(seq),
            };
            assert_eq!(filled, Some(expected), "{case}");

            // Nothing but the stand-ins at the switch between the first
            // frame and the last: two for the 5 ms that the first 10 ms frame
            // came later, one more when it came after that, and the 10 ms
            // mode's extra frame when there was nothing to fill it.
            let played = listener.played_at(sent[0].seq).unwrap();
            let last = listener.played_at(sent.last().unwrap().seq).unwrap();
            let between: Vec<Source> = listener
                .heard
                .iter()
                .filter(|heard| heard.at >= played && heard.at <= last)
                .filter_map(|heard| heard.pulled.map(|pulled| pulled.source))
                .collect();
            assert!(!between.contains(&Source::Silence), "{case}");
            let made_up = between.iter().filter(|&&s| s == Source::Concealed).count();
            assert!(made_up <= 3, "{case}: {made_up} frames made up");
            let stats = listener.stats();
            assert_eq!(
                (stats.late, stats.lost, stats.frame_ms, stats.depth_frames),
                (0, u64::from(delay.is_none()), 10, 2),
                "{case}"
            );
            listener.assert_in_order();
            listener.assert_no_clicks();
        }
    }
}

// Back to 5 ms frames the 10 ms mode's extra frame is not needed, and no
// late packet asked for it: it comes off in the next pauses instead of one
// frame per 5 s.
#[test]
fn back_from_the_10_ms_mode_its_extra_depth_comes_off() {
    let before = speech(2.0, -70.0);
    let after = speech(3.0, -70.0);
    let sent = talk(
        &[(Mode::Repair, &before), (Mode::LowDelay, &after)],
        0,
        0,
        false,
    );
    let mut listener = Listener::new();
    listener.run(&sent, &on_time(&sent), 5 * SECOND + TICK, always);
    for packet in sent.iter().filter(|packet| packet.sent_at >= 3 * SECOND) {
        assert_eq!(listener.played_at(packet.seq), Some(packet.sent_at));
    }
    let stats = listener.stats();
    assert_eq!((stats.frame_ms, stats.depth_frames), (5, 1));
    assert_eq!((stats.late, stats.lost, stats.dropped), (0, 0, 3));
    listener.assert_in_order();
    listener.assert_no_clicks();
}

// The talker's end mark is lost, so the buffer times out; then the listener
// deafens and the talker says something. Nobody pulling means nothing tells
// the buffer that time passes, and without the mixer's pause every frame
// that fell out of it would count as lost.
#[test]
fn pause_after_a_time_out_counts_no_loss() {
    let first = low_delay(&tone(1.0), 0);
    let second = talk(&[(Mode::LowDelay, &tone(2.0))], 200, 2 * SECOND, false);
    let mut sent = first.clone();
    sent.extend(second.iter().cloned());
    let arrive = on_time(&sent);
    let mut listener = Listener::new();
    listener.run(&sent, &arrive, 3 * SECOND / 2, always);
    assert!(!listener.stats().playing);

    listener.buffer.pause();
    listener.run(&sent, &arrive, 3 * SECOND, |_| false);
    let stats = listener.stats();
    assert_eq!((stats.lost, stats.late, stats.dropped), (0, 0, 0));
    assert_eq!(stats.loss_percent, 0.0);

    listener.run(&sent, &arrive, 4 * SECOND + TICK, always);
    for packet in second.iter().filter(|packet| packet.sent_at >= 3 * SECOND) {
        assert_eq!(listener.played_at(packet.seq), Some(packet.sent_at));
    }
    let stats = listener.stats();
    assert_eq!(
        (stats.lost, stats.late, stats.dropped, stats.depth_frames),
        (0, 0, 0, 1)
    );
    listener.assert_no_clicks();
}

// A microphone whose noise sits at -44 dBFS never makes a frame under
// QUIET_RMS, but its pauses are as quiet as it gets: the shrink happens in
// one of them.
#[test]
fn a_noisy_microphone_still_shrinks_in_its_pauses() {
    let sent = low_delay(&speech(12.0, -44.0), 0);
    let mut arrive = on_time(&sent);
    for i in [100, 110, 120, 130, 140] {
        arrive[i] = Some(sent[i].sent_at + TICK);
    }
    let mut listener = Listener::new();
    listener.run(&sent, &arrive, 2 * SECOND, always);
    assert_eq!(listener.stats().depth_frames, 2);

    listener.run(&sent, &arrive, 12 * SECOND, always);
    let stats = listener.stats();
    assert_eq!((stats.depth_frames, stats.dropped, stats.late), (1, 1, 5));
    let skipped: Vec<u64> = sent[200..sent.len() - 1]
        .iter()
        .filter(|packet| listener.played_at(packet.seq).is_none())
        .map(|packet| packet.sent_at)
        .collect();
    assert_eq!(skipped.len(), 1);
    assert!(
        in_a_pause(skipped[0]),
        "dropped the frame sent at {}",
        skipped[0]
    );
    listener.assert_no_clicks();
}

// A talker with no quiet frame at all, a steady tone, speaking in sentences:
// once a shrink is due it happens at the start of the next sentence, where
// the pause hides it.
#[test]
fn shrink_between_sentences() {
    let mut sent = Vec::new();
    for k in 0..4u64 {
        let mut sentence = talk(
            &[(Mode::LowDelay, &tone(2.0))],
            (k * 400) as u16,
            k * 3 * SECOND,
            false,
        );
        sentence.last_mut().unwrap().last = true;
        sent.extend(sentence);
    }
    let mut arrive = on_time(&sent);
    for i in [100, 110, 120, 130, 140] {
        arrive[i] = Some(sent[i].sent_at + TICK);
    }
    let mut listener = Listener::new();
    listener.run(&sent, &arrive, 11 * SECOND + 2 * TICK, always);

    // Grown in the first sentence at about 0.75 s. Only time spent playing
    // counts, so the 5 s are up during the third sentence, whose tone has
    // no quiet frame to drop.
    for packet in &sent[200..1200] {
        assert_eq!(listener.played_at(packet.seq), Some(packet.sent_at + TICK));
    }
    for packet in &sent[1200..] {
        assert_eq!(listener.played_at(packet.seq), Some(packet.sent_at));
    }
    let stats = listener.stats();
    assert_eq!((stats.depth_frames, stats.dropped, stats.lost), (1, 1, 0));
    listener.assert_no_clicks();
}

// The render side misses its turn for 30 ms. The play point falls that far
// behind, and without a late packet to back it the delay comes off in the
// next pauses, not one frame per 5 s.
#[test]
fn a_render_stall_comes_off_in_the_next_pauses() {
    let sent = low_delay(&speech(6.0, -70.0), 0);
    let stall = 2 * SECOND..2 * SECOND + 6 * TICK;
    let mut listener = Listener::new();
    listener.run(&sent, &on_time(&sent), 6 * SECOND + TICK, |now| {
        !stall.contains(&now)
    });
    let behind = sent
        .iter()
        .find(|packet| packet.sent_at > stall.end)
        .unwrap();
    assert_eq!(
        listener.played_at(behind.seq),
        Some(behind.sent_at + 6 * TICK)
    );
    // A drop plays the frame after the dropped one, so the next pause, ten
    // frames long, takes five of the six and the pause after it the last.
    let caught_up = 2 * SECOND + 2 * SYLLABLE + 6 * TICK;
    for packet in sent.iter().filter(|packet| packet.sent_at >= caught_up) {
        assert_eq!(listener.played_at(packet.seq), Some(packet.sent_at));
    }
    let stats = listener.stats();
    assert_eq!(
        (stats.depth_frames, stats.late, stats.lost, stats.dropped),
        (1, 0, 0, 6)
    );
    let skipped: Vec<u64> = sent[..sent.len() - 1]
        .iter()
        .filter(|packet| listener.played_at(packet.seq).is_none())
        .map(|packet| packet.sent_at)
        .collect();
    assert!(skipped.iter().all(|&at| in_a_pause(at)), "{skipped:?}");
    listener.assert_no_clicks();
}

// A 10 ms stall every 3 s, as a driver with a bad habit would give: each
// one comes off before the next, so they never add up.
#[test]
fn repeated_short_stalls_do_not_add_up() {
    let sent = low_delay(&speech(30.0, -70.0), 0);
    let mut listener = Listener::new();
    listener.run(&sent, &on_time(&sent), 30 * SECOND, |now| {
        now < SECOND || now % (3 * SECOND) >= 2 * TICK
    });
    for packet in sent.iter().filter(|packet| {
        let into = packet.sent_at % (3 * SECOND);
        packet.sent_at > SECOND && (SECOND..2 * SECOND).contains(&into)
    }) {
        assert_eq!(listener.played_at(packet.seq), Some(packet.sent_at));
    }
    let stats = listener.stats();
    assert_eq!((stats.depth_frames, stats.late, stats.lost), (1, 0, 0));
    listener.assert_no_clicks();
}

// Twenty sentences over a path with up to 10 ms of jitter. Each one starts
// from its first packet, and when that one happened to be slow, the start
// is late by that much; kept as the depth, that grew with every sentence
// until the buffer sat at the 60 ms cap. It has to stay near what one long
// transmission on the same path settles at: 10 ms.
#[test]
fn sentences_on_a_jittery_path_do_not_pile_up_delay() {
    let mut sent = Vec::new();
    for k in 0..20u64 {
        let mut sentence = talk(
            &[(Mode::LowDelay, &speech(2.0, -70.0))],
            (k * 400) as u16,
            k * 3 * SECOND,
            false,
        );
        sentence.last_mut().unwrap().last = true;
        sent.extend(sentence);
    }
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let arrive: Vec<Option<u64>> = sent
        .iter()
        .map(|packet| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            Some(packet.sent_at + state % (2 * TICK + 1))
        })
        .collect();
    let mut listener = Listener::new();
    listener.run(&sent, &arrive, 60 * SECOND, always);

    let delays: Vec<u64> = sent[10 * 400..]
        .iter()
        .filter_map(|packet| Some(listener.played_at(packet.seq)? - packet.sent_at))
        .collect();
    let mean_ms = delays.iter().sum::<u64>() as f64 / delays.len() as f64 / 48.0;
    let worst_ms = *delays.iter().max().unwrap() as f64 / 48.0;
    assert!(
        mean_ms <= 15.0 && worst_ms <= 25.0,
        "delay over the last ten sentences: mean {mean_ms:.1} ms, worst {worst_ms:.1} ms"
    );
    assert!(delays.len() > 3900, "{} of 4000 played", delays.len());
    listener.assert_in_order();
    listener.assert_no_clicks();
}

// A late first packet of a word, which once cut its start, as traced in the
// room's voice tests: packets reached the listener a median 0.5 ms after
// their frame was complete, and the first packet of a word 4.3 ms after.
// Here the talker's frames are complete 1.2 ms after one of the listener's
// pulls, so the rest arrive 3.3 ms before the pull that plays them, and a
// packet 4.3 ms late comes 0.5 ms after it, when its frame has been made up
// as lost.
const COMPLETE: u64 = 58;
const MEDIAN: u64 = 24;
const ONSET: u64 = 206;
// The room tests' level for a tone having reached the speakers.
const HEARD: f32 = 0.05;

// The room tests' tone. Its own largest step is 0.039, and 0.049 after the
// codec, which is past MAX_JOIN_STEP, so its join is checked against its
// own steps; the 330 Hz tone is the one clicks are measured with.
fn tone_1k(seconds: f32) -> Vec<f32> {
    let samples = (seconds * SAMPLE_RATE as f32) as usize;
    (0..samples)
        .map(|i| 0.3 * (TAU * 1000.0 * i as f32 / SAMPLE_RATE as f32).sin())
        .collect()
}

fn talked(signal: &[f32], redundancy: bool) -> Vec<Sent> {
    talk(&[(Mode::LowDelay, signal)], 7000, COMPLETE, redundancy)
}

// Half a second of digital silence, then `word`, which starts on a zero
// crossing at the start of a frame, as the room tests' tones do. Also the
// frame the word starts in.
fn a_word_after_silence(word: &[f32]) -> (Vec<Sent>, usize) {
    let mut signal = silence(0.5);
    let onset = signal.len() / TICK as usize;
    signal.extend_from_slice(word);
    (talked(&signal, false), onset)
}

// Every packet MEDIAN after its frame is complete, and those in `late` as
// long after it as they say.
fn listen(sent: &[Sent], late: &[(usize, u64)]) -> Listener {
    let mut arrive: Vec<Option<u64>> = sent
        .iter()
        .map(|packet| Some(packet.sent_at + MEDIAN))
        .collect();
    for &(i, after) in late {
        arrive[i] = Some(sent[i].sent_at + after);
    }
    let mut listener = Listener::new();
    let until = sent.last().unwrap().sent_at + 4 * TICK;
    listener.run(sent, &arrive, until, always);
    listener
}

// There is a pull every 5 ms from the start and each is 5 ms long, so an
// index into this is a time in samples.
fn audio(listener: &Listener) -> Vec<f32> {
    listener
        .heard
        .iter()
        .flat_map(|heard| heard.audio.iter().copied())
        .collect()
}

fn first_above(listener: &Listener, level: f32) -> Option<u64> {
    let at = audio(listener)
        .iter()
        .position(|sample| sample.abs() > level)?;
    Some(at as u64)
}

fn ms(samples: u64) -> f64 {
    samples as f64 * 1000.0 / SECOND as f64
}

// Each frame but the ones in `late` plays from its own packet at the first
// pull after it arrived, as it would with none late; the ones in `late`
// never play.
fn assert_on_time_but(listener: &Listener, sent: &[Sent], late: &[usize]) {
    for (i, packet) in sent.iter().enumerate() {
        let pull = (packet.sent_at + MEDIAN).next_multiple_of(TICK);
        let expected = (!late.contains(&i)).then_some(pull);
        assert_eq!(
            listener.played_at(packet.seq),
            expected,
            "seq {}",
            packet.seq
        );
    }
}

#[test]
fn late_first_packet_of_a_word() {
    for (name, word) in [("330 Hz", tone(0.5)), ("1 kHz", tone_1k(0.5))] {
        let (sent, onset) = a_word_after_silence(&word);
        let captured = COMPLETE + onset as u64 * TICK;
        let heard = |listener: &Listener| first_above(listener, HEARD).unwrap() - captured;
        let on_time = listen(&sent, &[]);
        let late = listen(&sent, &[(onset, ONSET)]);
        // A frame later still it cannot be decoded, which is how every late
        // frame was handled before.
        let too_late = listen(&sent, &[(onset, ONSET + TICK)]);
        // From the last sample before the made-up frame to the end of the
        // blended one after it.
        let made_up = (sent[onset].sent_at + MEDIAN).next_multiple_of(TICK) as usize;
        let blended = made_up + 2 * TICK as usize;
        let join = audio(&late)[made_up - 1..blended]
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).abs())
            .fold(0.0f32, f32::max);
        println!(
            "{name}: over {HEARD} {:.1} ms after its capture with the first packet on time, {:.1} ms with it {:.1} ms late, {:.1} ms when it cannot be decoded; largest step at the join {join:.4}, anywhere with it on time {:.4}",
            ms(heard(&on_time)),
            ms(heard(&late)),
            ms(ONSET),
            ms(heard(&too_late)),
            on_time.largest_step().0,
        );
        // The made-up frame costs the word's first 2.5 ms, which Opus's
        // lookahead put in that frame, and the next frame's blend in from
        // the made-up one reaches the level within 1 ms.
        assert!(
            heard(&late) <= heard(&on_time) + 168,
            "{name}: {:.1} ms",
            ms(heard(&late))
        );
        assert_on_time_but(&late, &sent, &[onset]);
        let stats = late.stats();
        assert_eq!(
            (
                stats.late,
                stats.late_decoded,
                stats.lost,
                stats.depth_frames
            ),
            (1, 1, 0, 1),
            "{name}"
        );
        // Past the blend the decoder is on the talker's history: what plays
        // is what plays with the first packet on time.
        let apart = audio(&on_time)[blended..]
            .iter()
            .zip(&audio(&late)[blended..])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(apart < 1e-4, "{name}: {apart} apart after the blend");
        late.assert_in_order();
        if name == "330 Hz" {
            late.assert_no_clicks();
        } else {
            assert!(join <= on_time.largest_step().0, "{name}: a step of {join}");
        }
    }
}

// Speech with packets 4.3 ms late at the start of a syllable, in the middle
// of one, and two in a row at the start of one. At most four in any 2 s, so
// the buffer keeps its one frame. Each is decoded after its stand-in, none
// plays, and each join is smooth. The end mark keeps the tail from counting
// as concealed.
#[test]
fn late_packets_in_speech() {
    let mut sent = talked(&speech(4.0, -44.0), false);
    sent.last_mut().unwrap().last = true;
    let late = [80, 137, 400, 401, 697, 760];
    let after: Vec<(usize, u64)> = late.iter().map(|&i| (i, ONSET)).collect();
    let listener = listen(&sent, &after);
    assert_on_time_but(&listener, &sent, &late);
    let stats = listener.stats();
    assert_eq!(
        (stats.late, stats.late_decoded, stats.lost, stats.concealed),
        (6, 6, 0, 6)
    );
    assert_eq!(
        (stats.depth_frames, stats.inserted, stats.dropped),
        (1, 0, 0)
    );
    listener.assert_in_order();
    listener.assert_no_clicks();
}

// A frame that comes after the frame behind it has played is too late for
// the decoder, which has moved on; a frame played from the copy the next
// packet carried was decoded already. Neither is decoded again or played.
#[test]
fn late_frame_decoded_only_while_in_reach() {
    let (sent, onset) = a_word_after_silence(&tone(0.5));
    let listener = listen(&sent, &[(onset, ONSET + TICK)]);
    assert_on_time_but(&listener, &sent, &[onset]);
    let stats = listener.stats();
    assert_eq!((stats.late, stats.late_decoded, stats.lost), (1, 0, 0));
    listener.assert_in_order();
    listener.assert_no_clicks();

    // Two frames deep with redundancy on: the copy in the next packet is in
    // before the frame's pull, and the frame itself after it.
    let sent = talked(&tone(1.0), true);
    let copied = 100;
    let seq = sent[copied].seq;
    let listener = listen(&sent, &[(copied, MEDIAN + 2 * TICK)]);
    let played: Vec<Source> = listener
        .sources()
        .into_iter()
        .filter(|source| matches!(source, Source::Packet(s) | Source::Copy(s) | Source::Repaired(s) if *s == seq))
        .collect();
    assert_eq!(played, [Source::Copy(seq)]);
    let stats = listener.stats();
    assert_eq!(
        (
            stats.late,
            stats.late_decoded,
            stats.redundancy_used,
            stats.lost
        ),
        (1, 0, 1, 0)
    );
    listener.assert_in_order();
    listener.assert_no_clicks();
}
