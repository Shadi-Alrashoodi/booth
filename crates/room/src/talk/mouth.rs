// Talking: the capture callback. It cuts the microphone's packets into Opus
// frames, decides frame by frame whether to send (Hold to talk, or the open
// mic detector), encodes, and sends the frame on every link itself. Both
// encoders are made before the microphone opens, so nothing here allocates
// once it runs, except the odd block of the channel that wakes the view.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use channels::Channel;
use voice::codec::{CodecError, Encoder, MAX_FRAME, MAX_PACKET, Mode, SAMPLE_RATE};
use voice::process::Mark;

use super::detect::Detector;
use super::wire::{Frame, MAX_VOICE};
use super::{Shared, TalkMode};
use crate::peer::Clock;

// The host's own voice goes out as Relayed from this slot: clients take no
// other kind.
pub(crate) const HOST_SLOT: u8 = 0;

pub(crate) struct Mouth {
    shared: Arc<Shared>,
    generation: u64,
    clock: Clock,
    low_delay: Encoder,
    repair: Encoder,
    mode: Mode,
    expected_loss: u8,
    detector: Detector,
    talk: TalkMode,
    sensitivity_db: i8,
    constant_rate: bool,
    pcm: [f32; MAX_FRAME],
    marks: [Mark; MAX_FRAME],
    filled: usize,
    // When the first sample in `pcm` was captured.
    first: Option<Instant>,
    sending: bool,
    // The last frame sent, for the copy the next packet carries.
    previous: [u8; MAX_PACKET],
    previous_len: usize,
    previous_seq: Option<u16>,
    opus: [u8; MAX_PACKET],
    payload: Vec<u8>,
    plain: Vec<u8>,
    sealed: Vec<u8>,
}

impl Mouth {
    // Off the capture thread: the encoders allocate.
    pub(crate) fn new(shared: Arc<Shared>, clock: Clock) -> Result<Mouth, CodecError> {
        let constant_rate = shared.constant_rate.load(Ordering::Relaxed);
        let sensitivity_db = shared.sensitivity_db.load(Ordering::Relaxed);
        Ok(Mouth {
            generation: shared.capture_generation.load(Ordering::Relaxed),
            low_delay: Encoder::new(Mode::LowDelay, constant_rate)?,
            repair: Encoder::new(Mode::Repair, constant_rate)?,
            mode: Mode::LowDelay,
            expected_loss: 0,
            detector: Detector::new().quietest(sensitivity_db),
            talk: shared.talk_mode(),
            sensitivity_db,
            constant_rate,
            pcm: [0.0; MAX_FRAME],
            marks: [Mark::Room; MAX_FRAME],
            filled: 0,
            first: None,
            sending: false,
            previous: [0; MAX_PACKET],
            previous_len: 0,
            previous_seq: None,
            opus: [0; MAX_PACKET],
            payload: Vec::with_capacity(MAX_VOICE),
            plain: Vec::with_capacity(MAX_VOICE + 1),
            sealed: Vec::with_capacity(MAX_VOICE + 1 + session::DATA_OVERHEAD),
            shared,
            clock,
        })
    }

    // The capture callback: `samples` is one device period of mono 48 kHz,
    // the first of it captured at `captured`, and `marks` what each sample
    // is to the open mic floor.
    pub(crate) fn hear(&mut self, samples: &[f32], marks: &[Mark], captured: Instant) {
        debug_assert_eq!(samples.len(), marks.len());
        let began = Instant::now();
        let mut at = 0;
        while at < samples.len() {
            if self.filled == 0 {
                self.first = Some(captured + samples_to_time(at));
            }
            let frame = self.mode.frame_samples();
            let take = (frame - self.filled).min(samples.len() - at);
            self.pcm[self.filled..self.filled + take].copy_from_slice(&samples[at..at + take]);
            self.marks[self.filled..self.filled + take].copy_from_slice(&marks[at..at + take]);
            self.filled += take;
            at += take;
            if self.filled == frame {
                self.filled = 0;
                self.frame();
                self.follow_room();
            }
        }
        self.shared.capture_times.record(began.elapsed());
        stats::gauge!("room.mouth.payload", self.payload);
        stats::gauge!("room.mouth.plain", self.plain);
        stats::gauge!("room.mouth.sealed", self.sealed);
    }

    // The microphone opened again after a gap. The part of a frame from
    // before it does not go out with what comes after, and a spell that was
    // going ended with the gap, as when the stream closes (see Drop). The
    // open mic floor is learnt again, from whichever microphone this is.
    pub(crate) fn restart(&mut self) {
        self.filled = 0;
        self.first = None;
        self.detector =
            Detector::new().quietest(self.shared.sensitivity_db.load(Ordering::Relaxed));
        if self.sending {
            self.sending = false;
            if self.generation == self.shared.capture_generation.load(Ordering::Relaxed) {
                self.shared.sending.store(false, Ordering::Relaxed);
                self.shared.changed();
            }
        }
    }

    fn frame(&mut self) {
        if self.generation != self.shared.capture_generation.load(Ordering::Relaxed) {
            return;
        }
        let frame = self.mode.frame_samples();
        let talk = self.shared.talk_mode();
        if talk != self.talk {
            self.talk = talk;
            if talk == TalkMode::OpenMic {
                self.detector = Detector::new().quietest(self.sensitivity_db);
            }
        }
        let sensitivity_db = self.shared.sensitivity_db.load(Ordering::Relaxed);
        if sensitivity_db != self.sensitivity_db {
            self.sensitivity_db = sensitivity_db;
            self.detector.set_quietest(sensitivity_db);
        }
        let heard = match talk {
            TalkMode::PushToTalk => self.shared.held.load(Ordering::Relaxed),
            // The detector hears every frame, sent or not, so its floor is
            // the room's and not only the voice's.
            TalkMode::OpenMic => self
                .detector
                .wanted(&self.pcm[..frame], &self.marks[..frame]),
        };
        let wanted = heard && self.shared.microphone_wanted();
        if !wanted && !self.sending {
            return;
        }
        if !self.sending {
            let constant_rate = self.shared.constant_rate.load(Ordering::Relaxed);
            if constant_rate != self.constant_rate {
                let _ = self.low_delay.set_constant_rate(constant_rate);
                let _ = self.repair.set_constant_rate(constant_rate);
                self.constant_rate = constant_rate;
            }
            // A spell starts from a reset encoder, as the far side's decoder
            // does after the end of the one before.
            let _ = self.encoder().reset();
            // The key goes down anywhere in the microphone's waveform, and
            // Opus would give that cut back as a click. Faded in over the
            // first frame, the spell starts from silence.
            for (i, sample) in self.pcm[..frame].iter_mut().enumerate() {
                *sample *= (i + 1) as f32 / frame as f32;
            }
            self.previous_seq = None;
            self.sending = true;
            self.shared.sending.store(true, Ordering::SeqCst);
            // The microphone may have been let go since the check above.
            if self.generation != self.shared.capture_generation.load(Ordering::SeqCst) {
                self.shared.sending.store(false, Ordering::SeqCst);
            }
            self.shared.changed();
        }
        let last = !wanted;
        if last && !self.shared.microphone_wanted() {
            // Mute or Deafen in the middle of a spell. Part of this frame
            // was heard after the click; faded out, it goes at a fraction of
            // its level, and the spell ends in silence as it began.
            for (i, sample) in self.pcm[..frame].iter_mut().enumerate() {
                *sample *= (frame - 1 - i) as f32 / frame as f32;
            }
        }
        let size = self.mode.packet_bytes();
        let encoder = match self.mode {
            Mode::LowDelay => &mut self.low_delay,
            Mode::Repair => &mut self.repair,
        };
        let Ok(len) = encoder.encode(&self.pcm[..frame], &mut self.opus[..size]) else {
            return;
        };
        self.send(len, last);
        if last {
            self.sending = false;
            self.shared.sending.store(false, Ordering::Relaxed);
            self.shared.changed();
        }
    }

    fn send(&mut self, len: usize, last: bool) {
        let shared = &self.shared;
        let seq = shared.next_seq.fetch_add(1, Ordering::Relaxed);
        // In the 10 ms mode Opus's own repair data does this job.
        let copy = self.mode == Mode::LowDelay && shared.redundancy.load(Ordering::Relaxed);
        let previous = (copy && self.previous_seq == Some(seq.wrapping_sub(1)))
            .then(|| &self.previous[..self.previous_len]);
        // At a constant rate the first packet of a spell carries padding
        // where the copy would be, so it is no shorter than the rest.
        let pad = if copy && previous.is_none() && self.constant_rate {
            self.mode.packet_bytes()
        } else {
            0
        };
        let captured = self.first.map_or_else(
            || self.clock.micros(Instant::now()),
            |at| self.clock.micros(at),
        );
        let frame = Frame {
            seq,
            captured,
            mode: self.mode,
            redundancy: copy,
            last,
            frame: &self.opus[..len],
            previous,
            pad,
        };
        if shared.host {
            frame.write_relayed(HOST_SLOT, Some(captured), false, &mut self.payload);
        } else {
            frame.write_spoken(&mut self.payload);
        }
        self.plain.clear();
        channels::frame(Channel::Voice, &self.payload, &mut self.plain);
        let route = shared.route();
        if let Some(socket) = &route.socket {
            for outlet in &route.outlets {
                if outlet.sealer.seal(&self.plain, &mut self.sealed).is_err() {
                    continue;
                }
                // A failed send is a dead route; the room hears about that
                // from the silence, as with every other packet.
                if socket.send_to(&self.sealed, outlet.to).is_ok() {
                    outlet.sent.count(self.sealed.len());
                }
            }
        }
        shared.frames_sent.fetch_add(1, Ordering::Relaxed);
        shared.last_sent_us.store(captured, Ordering::Relaxed);
        self.previous[..len].copy_from_slice(&self.opus[..len]);
        self.previous_len = len;
        self.previous_seq = Some(seq);
    }

    // What the loss reports decided, between frames. A switch changes the
    // size of the next frame; the frame before is still sent as a copy, in
    // its own size.
    fn follow_room(&mut self) {
        let shared = &self.shared;
        let expected = shared.expected_loss.load(Ordering::Relaxed);
        if expected != self.expected_loss {
            self.expected_loss = expected;
            let _ = self.low_delay.set_expected_loss(expected);
            let _ = self.repair.set_expected_loss(expected);
        }
        let mode = if shared.repair.load(Ordering::Relaxed) {
            Mode::Repair
        } else {
            Mode::LowDelay
        };
        if mode != self.mode {
            self.mode = mode;
            // The other encoder last ran before the switch before this one.
            let _ = self.encoder().reset();
        }
    }

    fn encoder(&mut self) -> &mut Encoder {
        match self.mode {
            Mode::LowDelay => &mut self.low_delay,
            Mode::Repair => &mut self.repair,
        }
    }
}

// Mute, Deafen or a microphone that went away usually closes the stream
// between two frames, so no frame ends the spell. Nothing may go on saying
// this PC is sending.
impl Drop for Mouth {
    fn drop(&mut self) {
        if self.sending && self.generation == self.shared.capture_generation.load(Ordering::Relaxed)
        {
            self.shared.sending.store(false, Ordering::Relaxed);
            self.shared.changed();
        }
    }
}

fn samples_to_time(samples: usize) -> Duration {
    Duration::from_nanos(samples as u64 * 1_000_000_000 / u64::from(SAMPLE_RATE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::talk::{VoiceConfig, read_relayed, read_spoken};
    use voice::process::{Processor, Switches};

    fn mouth(talk: TalkMode, constant_rate: bool, host: bool) -> (Mouth, Arc<Shared>) {
        let config = VoiceConfig {
            talk,
            constant_rate,
            ..VoiceConfig::default()
        };
        let shared = Shared::new(&config, None, host);
        let clock = Clock::new(Instant::now());
        (Mouth::new(Arc::clone(&shared), clock).unwrap(), shared)
    }

    // What went out, read back through the far side's parser. With no
    // socket the frames are taken from the payload after each one.
    // `at` is the capture time of the next period, moved on by each one.
    fn run(mouth: &mut Mouth, at: &mut Instant, periods: usize, signal: f32) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for _ in 0..periods {
            let before = mouth.shared.frames_sent.load(Ordering::Relaxed);
            let samples = [signal; 128];
            mouth.hear(&samples, &[Mark::Room; 128], *at);
            *at += samples_to_time(128);
            if mouth.shared.frames_sent.load(Ordering::Relaxed) != before {
                out.push(mouth.payload.clone());
            }
        }
        out
    }

    #[test]
    fn hold_to_talk_marks_last_frame() {
        let (mut mouth, shared) = mouth(TalkMode::PushToTalk, true, false);
        let mut at = Instant::now();
        assert!(run(&mut mouth, &mut at, 100, 0.2).is_empty());
        shared.set_held(true);
        // 128-sample periods make 240-sample frames: 15 frames per 8 periods.
        let sent = run(&mut mouth, &mut at, 80, 0.2);
        assert!(shared.sending());
        shared.set_held(false);
        let tail = run(&mut mouth, &mut at, 8, 0.2);
        assert!(!shared.sending());
        // 80 periods of 128 samples, and what was left over from before.
        assert!((42..=43).contains(&sent.len()), "{}", sent.len());
        assert_eq!(tail.len(), 1, "one last frame after the release");
        let frames: Vec<Frame> = sent
            .iter()
            .chain(&tail)
            .map(|p| read_spoken(p).unwrap())
            .collect();
        for pair in frames.windows(2) {
            assert_eq!(pair[1].seq, pair[0].seq.wrapping_add(1));
            assert!((pair[1].captured - pair[0].captured).abs_diff(5000) <= 1);
        }
        assert!(frames[..frames.len() - 1].iter().all(|f| !f.last));
        assert!(frames.last().unwrap().last);
        assert!(
            frames
                .iter()
                .all(|f| f.frame.len() == 20 && f.mode == Mode::LowDelay)
        );
        assert!(run(&mut mouth, &mut at, 100, 0.2).is_empty());
    }

    #[test]
    fn live_talk_mode_and_sensitivity_keep_the_microphone() {
        let (mut mouth, shared) = mouth(TalkMode::PushToTalk, true, false);
        let mut at = Instant::now();
        let mut config = VoiceConfig {
            talk: TalkMode::OpenMic,
            sensitivity_db: -30,
            ..VoiceConfig::default()
        };
        shared.set_voice(&config);
        assert!(run(&mut mouth, &mut at, 750, 0.0003).is_empty());
        let floor = mouth.detector.floor_db();
        assert!(run(&mut mouth, &mut at, 20, 0.01).is_empty());
        config.sensitivity_db = -50;
        shared.set_voice(&config);
        assert!(!run(&mut mouth, &mut at, 20, 0.01).is_empty());
        assert!((mouth.detector.floor_db() - floor).abs() < 1.0);
        config.talk = TalkMode::PushToTalk;
        shared.set_voice(&config);
        let tail = run(&mut mouth, &mut at, 20, 0.1);
        assert_eq!(tail.len(), 1);
        assert!(read_spoken(&tail[0]).unwrap().last);
        assert!(!shared.sending());
        assert!(run(&mut mouth, &mut at, 20, 0.1).is_empty());
        assert_eq!(shared.view().mode, TalkMode::PushToTalk);
    }

    #[test]
    fn live_constant_rate_waits_for_the_next_spell() {
        let (mut mouth, shared) = mouth(TalkMode::PushToTalk, true, false);
        let mut at = Instant::now();
        shared.redundancy.store(true, Ordering::Relaxed);
        shared.set_held(true);
        let first = run(&mut mouth, &mut at, 20, 0.2);
        assert_eq!(read_spoken(&first[0]).unwrap().pad, 20);
        shared.set_voice(&VoiceConfig {
            constant_rate: false,
            ..VoiceConfig::default()
        });
        let same = run(&mut mouth, &mut at, 20, 0.2);
        assert!(mouth.constant_rate);
        assert!(
            same.iter()
                .all(|bytes| read_spoken(bytes).unwrap().frame.len() == 20)
        );
        shared.set_held(false);
        run(&mut mouth, &mut at, 20, 0.2);
        shared.set_held(true);
        let next = run(&mut mouth, &mut at, 100, 0.0);
        assert!(!mouth.constant_rate);
        assert_eq!(read_spoken(&next[0]).unwrap().pad, 0);
        assert!(
            next.iter()
                .any(|bytes| read_spoken(bytes).unwrap().frame.len() < 20)
        );
    }

    #[test]
    fn a_retired_microphone_cannot_end_the_new_spell() {
        let (mut old, shared) = mouth(TalkMode::PushToTalk, true, false);
        let mut at = Instant::now();
        shared.set_held(true);
        run(&mut old, &mut at, 20, 0.2);
        shared.capture_generation.fetch_add(1, Ordering::Relaxed);
        let mut next = Mouth::new(Arc::clone(&shared), Clock::new(at)).unwrap();
        run(&mut next, &mut at, 20, 0.2);
        assert!(run(&mut old, &mut at, 20, 0.2).is_empty());
        old.restart();
        assert!(shared.sending());
        drop(old);
        assert!(shared.sending());
    }

    #[test]
    fn muted_sends_nothing_even_while_held() {
        let (mut mouth, shared) = mouth(TalkMode::PushToTalk, true, false);
        let mut at = Instant::now();
        shared.set_held(true);
        assert!(!shared.set_muted(true));
        assert!(run(&mut mouth, &mut at, 50, 0.2).is_empty());
        assert!(shared.set_muted(false));
        assert!(!run(&mut mouth, &mut at, 50, 0.2).is_empty());
        assert!(!shared.set_deafened(true));
        let tail = run(&mut mouth, &mut at, 50, 0.2);
        assert_eq!(tail.len(), 1);
        assert!(read_spoken(&tail[0]).unwrap().last);
    }

    // A frame that is under way at the click ends the spell, faded out, where
    // Hold to talk let go ends it at full level.
    #[test]
    fn mute_fades_last_frame() {
        let (mut mouth, shared) = mouth(TalkMode::PushToTalk, true, false);
        let mut at = Instant::now();
        shared.set_held(true);
        // One 240-sample period a frame, so the frame sent is still in pcm.
        let mut frame = |mouth: &mut Mouth| {
            let before = mouth.shared.frames_sent.load(Ordering::Relaxed);
            mouth.hear(&[0.2; 240], &[Mark::Room; 240], at);
            at += samples_to_time(240);
            assert_eq!(mouth.shared.frames_sent.load(Ordering::Relaxed), before + 1);
            read_spoken(&mouth.payload).unwrap().last
        };
        for _ in 0..10 {
            assert!(!frame(&mut mouth));
        }
        assert!(mouth.pcm[..240].iter().all(|&s| s == 0.2));
        assert!(!shared.set_muted(true));
        assert!(frame(&mut mouth), "the last frame says so");
        let faded = &mouth.pcm[..240];
        assert!(
            (faded[0] - 0.2).abs() < 0.001 && faded[239] == 0.0,
            "{faded:?}"
        );
        assert!(faded.windows(2).all(|pair| pair[1] < pair[0]));
        assert!(!shared.sending());
    }

    #[test]
    fn closed_microphone_stops_sending() {
        let (mut mouth, shared) = mouth(TalkMode::PushToTalk, true, false);
        let mut at = Instant::now();
        shared.set_held(true);
        assert!(!run(&mut mouth, &mut at, 10, 0.2).is_empty());
        assert!(shared.sending());
        shared.set_muted(true);
        drop(mouth);
        assert!(!shared.sending());
    }

    // A microphone opened again, perhaps another one, learns its own open mic
    // floor: the steady room of a louder microphone after a quiet one does
    // not send.
    #[test]
    fn reopened_microphone_learns_its_floor() {
        let (mut mouth, shared) = mouth(TalkMode::OpenMic, true, false);
        let mut at = Instant::now();
        // About -70 dBFS, then -45.
        assert!(run(&mut mouth, &mut at, 750, 0.0003).is_empty());
        mouth.restart();
        assert!(run(&mut mouth, &mut at, 750, 0.0056).is_empty());
        assert!(!shared.sending());
    }

    // A spell the gap cut off is over: the microphone opened again sends no
    // tail of it, whatever it hears.
    #[test]
    fn reopened_microphone_sends_no_tail() {
        let (mut mouth, shared) = mouth(TalkMode::OpenMic, true, false);
        let mut at = Instant::now();
        // About -70 dBFS, then a voice at -20.
        assert!(run(&mut mouth, &mut at, 750, 0.0003).is_empty());
        assert!(!run(&mut mouth, &mut at, 10, 0.1).is_empty());
        assert!(shared.sending());
        mouth.restart();
        assert!(run(&mut mouth, &mut at, 750, 0.0003).is_empty());
        assert!(!shared.sending());
    }

    // A microphone opened again keeps the open mic sensitivity from
    // Settings: a voice at -40 dBFS that -30 turns away is still turned away,
    // and one at -20 still goes.
    #[test]
    fn reopened_microphone_keeps_the_sensitivity() {
        let config = VoiceConfig {
            talk: TalkMode::OpenMic,
            sensitivity_db: -30,
            ..VoiceConfig::default()
        };
        let shared = Shared::new(&config, None, false);
        let mut mouth = Mouth::new(Arc::clone(&shared), Clock::new(Instant::now())).unwrap();
        let mut at = Instant::now();
        // About -70 dBFS on both sides of the gap.
        assert!(run(&mut mouth, &mut at, 750, 0.0003).is_empty());
        mouth.restart();
        assert!(run(&mut mouth, &mut at, 750, 0.0003).is_empty());
        assert!(run(&mut mouth, &mut at, 50, 0.01).is_empty());
        assert!(!run(&mut mouth, &mut at, 10, 0.1).is_empty());
    }

    // White noise at `db` dBFS RMS, a fan or a room, for `samples` samples.
    fn room(samples: usize, db: f32, seed: &mut u32) -> Vec<f32> {
        let amplitude = 10f32.powf(db / 20.0) * 3f32.sqrt();
        (0..samples)
            .map(|_| {
                *seed ^= *seed << 13;
                *seed ^= *seed >> 17;
                *seed ^= *seed << 5;
                amplitude * (*seed as f32 / u32::MAX as f32 * 2.0 - 1.0)
            })
            .collect()
    }

    // `signal` through the microphone's processing into the mouth, in the
    // usual 128-sample periods, as the capture callback does.
    fn speak(mouth: &mut Mouth, processor: &mut Processor, signal: &[f32], at: &mut Instant) {
        for period in signal.chunks(128) {
            let (samples, marks, captured) = processor.run_marked(period, *at);
            mouth.hear(samples, marks, captured);
            *at += samples_to_time(period.len());
        }
    }

    fn sent(mouth: &Mouth) -> u64 {
        mouth.shared.frames_sent.load(Ordering::Relaxed)
    }

    // A mute that leaves dither at -90 dBFS sends none of the room after it:
    // a block of it on the blocks or 8 samples off them, 2 s of it, or 2 s
    // of it while automatic volume holds a quiet microphone at +20 dB.
    #[test]
    fn dither_mute_sends_nothing() {
        let mut seed = 0x2545_f491;
        for (before, dither) in [(96_000, 240), (96_008, 240), (96_000, 96_000)] {
            let mut signal = room(before, -45.0, &mut seed);
            signal.extend(room(dither, -90.0, &mut seed));
            signal.extend(room(5 * 48_000, -45.0, &mut seed));
            let (mut talker, _) = mouth(TalkMode::OpenMic, true, false);
            let mut processor = Processor::new(Arc::new(Switches::new(false, true)));
            speak(&mut talker, &mut processor, &signal, &mut Instant::now());
            assert_eq!(
                sent(&talker),
                0,
                "{dither} samples of dither after {before}"
            );
        }

        let mut seed = 0x1234_5678;
        let mut signal = room(6 * 48_000, -65.0, &mut seed);
        let words = 10f32.powf(-40.0 / 20.0) * 2f32.sqrt();
        for (n, sample) in signal.iter_mut().enumerate() {
            if n % 28_800 < 19_200 {
                *sample += words * (std::f32::consts::TAU * 440.0 * n as f32 / 48_000.0).sin();
            }
        }
        signal.extend(room(5 * 48_000, -65.0, &mut seed));
        let (mut talker, _) = mouth(TalkMode::OpenMic, true, false);
        let mut processor = Processor::new(Arc::new(Switches::new(false, true)));
        let mut at = Instant::now();
        speak(&mut talker, &mut processor, &signal, &mut at);
        assert!(processor.gain_db() > 15.0, "{} dB", processor.gain_db());
        let before = sent(&talker);
        let mut after = room(2 * 48_000, -90.0, &mut seed);
        after.extend(room(10 * 48_000, -65.0, &mut seed));
        speak(&mut talker, &mut processor, &after, &mut at);
        assert_eq!(sent(&talker), before);
    }

    // Opened on a mute, open mic has learnt no floor when the room comes
    // back, and the block the unmute falls in is no voice: after dither,
    // after zeros that end early in a block, and with noise suppression on
    // after zeros at the open or a mute of 2 s.
    #[test]
    fn mute_first_sends_nothing() {
        let mut sends = Vec::new();
        for (zeros, len, suppressing, db) in [
            (false, 2 * 48_000 + 100, false, -45.0),
            (true, 256, false, -45.0),
            (true, 128, true, -45.0),
            (true, 2 * 48_000 + 100, true, -45.0),
            (false, 2 * 48_000 + 300, true, -40.0),
        ] {
            let mut signal = if zeros {
                vec![0.0; len]
            } else {
                room(len, -90.0, &mut 0x51ed_270b)
            };
            signal.extend(room(3 * 48_000, db, &mut 0x2545_f491));
            let (mut talker, _) = mouth(TalkMode::OpenMic, true, false);
            let mut processor = Processor::new(Arc::new(Switches::new(suppressing, true)));
            speak(&mut talker, &mut processor, &signal, &mut Instant::now());
            sends.push(sent(&talker));
        }
        assert_eq!(sends, [0; 5]);
    }

    // A microphone with its own gate gives exact zeros until a word, and the
    // block the first word starts in still starts a spell.
    #[test]
    fn gate_first_word_sent() {
        let mut signal = vec![0.0; 48_000 + 100];
        let word = |n: usize| 0.1 * (std::f32::consts::TAU * 300.0 * n as f32 / 48_000.0).sin();
        signal.extend((0..12_000).map(word));
        signal.extend(vec![0.0; 12_000]);
        let (mut talker, _) = mouth(TalkMode::OpenMic, true, false);
        let mut processor = Processor::new(Arc::new(Switches::new(false, true)));
        speak(&mut talker, &mut processor, &signal, &mut Instant::now());
        assert!(sent(&talker) > 0);
    }

    // With noise suppression on, its fades into a mute and out of it, and
    // RNNoise's unsettled blocks after, are not the open mic floor: the room
    // after a mute sends no more than the same room with no mute in it.
    #[test]
    fn suppression_fades_send_nothing() {
        let mut seed = 0x2545_f491;
        let plain = room(6 * 48_000, -40.0, &mut seed);
        let dither = room(4_800, -90.0, &mut 0x51ed_270b);
        for (at, dithered) in [
            (96_000, false),
            (96_312, false),
            (96_000, true),
            (96_096, true),
        ] {
            let mut muted = plain.clone();
            if dithered {
                muted[at..at + 4_800].copy_from_slice(&dither);
            } else {
                muted[at..at + 4_800].fill(0.0);
            }
            let switches = Arc::new(Switches::new(true, false));
            let (mut talker, _) = mouth(TalkMode::OpenMic, true, false);
            let (mut control, _) = mouth(TalkMode::OpenMic, true, false);
            talker.detector = Detector::with_random(|| 0);
            control.detector = Detector::with_random(|| 0);
            let mut processor = Processor::new(Arc::clone(&switches));
            let mut control_processor = Processor::new(switches);
            let (mut at_talker, mut at_control) = (Instant::now(), Instant::now());
            let (mut fed, mut worst, mut from) = (0, 0.0f32, None);
            for (period, same) in muted.chunks(128).zip(plain.chunks(128)) {
                if fed + period.len() > at {
                    from.get_or_insert((sent(&talker), sent(&control)));
                }
                speak(&mut talker, &mut processor, period, &mut at_talker);
                speak(&mut control, &mut control_processor, same, &mut at_control);
                fed += period.len();
                if (at..at + 2 * 48_000).contains(&fed) {
                    worst = worst.min(talker.detector.floor_db() - control.detector.floor_db());
                }
            }
            let (from, from_control) = from.unwrap();
            let (more, more_control) = (sent(&talker) - from, sent(&control) - from_control);
            let what = format!("{} at {at}", if dithered { "dither" } else { "zeros" });
            println!(
                "{what}: floor at worst {worst:.1} dB from the control's, {more} frames sent against {more_control}"
            );
            assert!(worst >= -3.0, "{what}: {worst} dB");
            assert!(more <= more_control, "{what}: {more} frames");
        }
    }

    // Nothing of the room goes out while noise suppression settles: from the
    // first sample, after a reopen, or once it is turned on.
    #[test]
    fn suppression_start_sends_nothing() {
        let mut seed = 0x2545_f491;
        let mut next = |seconds: usize, db| room(seconds * 48_000, db, &mut seed);
        let switches = Arc::new(Switches::new(true, true));
        let mut at = Instant::now();
        let (mut talker, _) = mouth(TalkMode::OpenMic, true, false);
        let mut processor = Processor::new(Arc::clone(&switches));
        speak(&mut talker, &mut processor, &next(2, -40.0), &mut at);
        let first = (sent(&talker), processor.gain_db());
        assert_eq!(first, (0, 0.0), "from the first sample");

        let (mut talker, _) = mouth(TalkMode::OpenMic, true, false);
        let mut processor = Processor::new(Arc::clone(&switches));
        speak(&mut talker, &mut processor, &next(2, -60.0), &mut at);
        processor.restart();
        talker.restart();
        let before = sent(&talker);
        speak(&mut talker, &mut processor, &next(4, -40.0), &mut at);
        let reopened = (sent(&talker) - before, processor.gain_db());
        assert_eq!(reopened, (0, 0.0), "after a reopen");

        switches.noise_suppression.store(false, Ordering::Relaxed);
        let (mut talker, _) = mouth(TalkMode::OpenMic, true, false);
        let mut processor = Processor::new(Arc::clone(&switches));
        speak(&mut talker, &mut processor, &next(2, -40.0), &mut at);
        switches.noise_suppression.store(true, Ordering::Relaxed);
        speak(&mut talker, &mut processor, &next(2, -40.0), &mut at);
        let turned_on = (sent(&talker), processor.gain_db());
        assert_eq!(turned_on, (0, 0.0), "turned on");
    }

    #[test]
    fn redundant_packets_same_size() {
        let (mut mouth, shared) = mouth(TalkMode::PushToTalk, true, false);
        let mut at = Instant::now();
        shared.redundancy.store(true, Ordering::Relaxed);
        shared.set_held(true);
        let sent = run(&mut mouth, &mut at, 40, 0.2);
        let sizes: Vec<usize> = sent.iter().map(Vec::len).collect();
        assert!(sizes.iter().all(|&size| size == sizes[0]), "{sizes:?}");
        let first = read_spoken(&sent[0]).unwrap();
        assert_eq!(
            (first.previous, first.pad, first.redundancy),
            (None, 20, true)
        );
        let second = read_spoken(&sent[1]).unwrap();
        assert_eq!(second.previous, Some(first.frame));
    }

    #[test]
    fn the_room_switches_frames_between_them() {
        let (mut mouth, shared) = mouth(TalkMode::PushToTalk, true, false);
        let mut at = Instant::now();
        shared.set_held(true);
        run(&mut mouth, &mut at, 10, 0.2);
        shared.repair.store(true, Ordering::Relaxed);
        shared.redundancy.store(true, Ordering::Relaxed);
        let sent = run(&mut mouth, &mut at, 40, 0.2);
        let frames: Vec<Frame> = sent.iter().map(|p| read_spoken(p).unwrap()).collect();
        let tens = frames.iter().filter(|f| f.mode == Mode::Repair).count();
        assert!(tens >= frames.len() - 1, "{} of {}", tens, frames.len());
        // The 10 ms mode carries Opus's own repair data instead of a copy.
        assert!(
            frames
                .iter()
                .filter(|f| f.mode == Mode::Repair)
                .all(|f| f.previous.is_none() && !f.redundancy)
        );
        assert!(
            frames
                .iter()
                .filter(|f| f.mode == Mode::Repair)
                .all(|f| f.frame.len() == 40)
        );
    }

    #[test]
    fn host_voice_from_host_slot() {
        let (mut mouth, shared) = mouth(TalkMode::PushToTalk, true, true);
        let mut at = Instant::now();
        shared.set_held(true);
        let sent = run(&mut mouth, &mut at, 10, 0.2);
        let relayed = read_relayed(&sent[0]).unwrap();
        assert_eq!(relayed.slot, HOST_SLOT);
        assert!(relayed.captured().is_some());
    }

    #[test]
    fn silence_shorter_without_constant_rate() {
        let (mut mouth, shared) = mouth(TalkMode::PushToTalk, false, false);
        let mut at = Instant::now();
        shared.set_held(true);
        let silence = run(&mut mouth, &mut at, 40, 0.0);
        let sizes: Vec<usize> = silence
            .iter()
            .map(|p| read_spoken(p).unwrap().frame.len())
            .collect();
        assert!(sizes.iter().all(|&size| size < 20), "{sizes:?}");
    }

    // Mouth::send alone, the part of the voice path after the encoder: the
    // links taken, the frame sealed for each and sent over loopback to two
    // of them. Printed, with the taking of the links timed on its own as
    // well; it fails only when the median send takes a tenth of a frame,
    // which a busy PC running every test at once does not reach.
    #[test]
    fn send_to_two_links_timing() {
        use crate::log::Log;
        use crate::socket::Socket;
        use crate::talk::{Outlet, Route};
        use crate::testing::{Wire, session_pair};

        const FRAMES: usize = 20_000;
        const LOOKUPS: u32 = 1_000_000;
        let socket = Arc::new(Socket::bind(0, Log::off()).expect("bind a socket"));
        let wires = [Wire::new(), Wire::new()];
        let shared = Shared::new(&VoiceConfig::default(), None, false);
        let outlets: Vec<Outlet> = wires
            .iter()
            .map(|wire| Outlet {
                sealer: session_pair().0.sealer().expect("the initiator may send"),
                to: wire.addr(),
                sent: Arc::default(),
            })
            .collect();
        *super::super::lock(&shared.route) = Arc::new(Route {
            socket: Some(socket),
            outlets,
        });
        let mut mouth = Mouth::new(Arc::clone(&shared), Clock::new(Instant::now())).unwrap();
        mouth.opus[..20].fill(0x55);

        let mut took = Vec::with_capacity(FRAMES);
        for _ in 0..FRAMES {
            let began = Instant::now();
            mouth.send(20, false);
            took.push(began.elapsed());
        }
        took.sort_unstable();
        let at = |share: f64| took[((FRAMES - 1) as f64 * share).round() as usize];
        let began = Instant::now();
        for _ in 0..LOOKUPS {
            std::hint::black_box(shared.route());
        }
        let lookup = began.elapsed().as_secs_f64() * 1e9 / f64::from(LOOKUPS);
        let build = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        let us = |duration: Duration| duration.as_secs_f64() * 1e6;
        println!(
            "{build} build, a frame to two links over loopback, {FRAMES} frames: median {:.2} us, p99 {:.2} us, p99.9 {:.2} us; taking the links {lookup:.1} ns",
            us(at(0.5)),
            us(at(0.99)),
            us(at(0.999))
        );
        assert!(wires.iter().all(|wire| !wire.packets().is_empty()));
        assert!(at(0.5) < Duration::from_micros(500), "{:?}", at(0.5));
    }

    // Hold to talk goes down wherever the microphone is in its waveform, here
    // at the top of a 330 Hz tone at 0.4. Decoded without any fade on the
    // listener's side, the spell must still start without a step larger than
    // the one voice/tests/jitter.rs allows at a join.
    #[test]
    fn spell_starts_without_click() {
        use voice::codec::Decoder;
        let (mut mouth, shared) = mouth(TalkMode::PushToTalk, true, false);
        shared.set_held(true);
        let start = Instant::now();
        let peak = 48_000 / 330 / 4;
        let mut sent = Vec::new();
        for n in 0..40 {
            let samples: Vec<f32> = (0..128)
                .map(|i| {
                    let t = (peak + n * 128 + i) as f32 / 48_000.0;
                    0.4 * (std::f32::consts::TAU * 330.0 * t).sin()
                })
                .collect();
            let before = shared.frames_sent.load(Ordering::Relaxed);
            mouth.hear(
                &samples,
                &[Mark::Room; 128],
                start + samples_to_time(n * 128),
            );
            if shared.frames_sent.load(Ordering::Relaxed) != before {
                sent.push(mouth.payload.clone());
            }
        }
        let mut decoder = Decoder::new().unwrap();
        let mut out = Vec::new();
        for packet in &sent {
            let frame = read_spoken(packet).unwrap();
            let mut pcm = [0f32; 480];
            let len = decoder.decode(frame.frame, &mut pcm).unwrap();
            out.extend_from_slice(&pcm[..len]);
        }
        let step = out
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).abs())
            .fold(0.0, f32::max);
        println!("largest step in a spell that starts at the top of the tone: {step:.3}");
        assert!(step <= 0.04, "{step}");
    }
}
