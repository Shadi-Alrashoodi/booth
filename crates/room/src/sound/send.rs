// Sending PC sound: the capture callback. It cuts each packet the capture
// gives into 10 ms stereo frames by sample count, keeps to the share's gate,
// starts and ends spells, encodes, and seals and sends each frame on every
// link itself, on the capture's own thread, as talk's mouth does for the
// microphone. The room takes the same lock for one thing only: to end the
// spell under way when the share pauses or the switch goes off, before it
// lets the capture go.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use channels::Channel;
use voice::stereo::{CHANNELS, Encoder, FRAME, PACKET_BYTES, SAMPLE_RATE, StereoError};

use super::{Gate, MAX_SOUND, Outgoing, Route, SoundFrame};
use crate::talk::HOST_SLOT;

// -90 dBFS. Under it on both channels a frame is silence, which a program's
// output is exactly while nothing plays.
const QUIET: f32 = 3.162_278e-5;
// 200 ms of it ends a spell, the last of them with LAST, and nothing goes
// out until a louder frame starts the next one.
const QUIET_FRAMES: u32 = 20;
const FRAME_TIME: Duration = Duration::from_millis(10);

pub(crate) struct Feed {
    out: Arc<Outgoing>,
    encoder: Encoder,
    // The gate the frame under way was cut under.
    gate: Option<Gate>,
    pcm: [f32; FRAME * CHANNELS],
    // Frames of left and right in `pcm`.
    filled: usize,
    // When the first of them was captured.
    first: Option<Instant>,
    // A spell is under way: its first frame went out from a reset encoder,
    // and its last has not.
    spell: bool,
    // Silent frames in a row within it.
    quiet: u32,
    opus: [u8; PACKET_BYTES],
    payload: Vec<u8>,
    plain: Vec<u8>,
    sealed: Vec<u8>,
}

impl Feed {
    // Off the capture thread: the encoder allocates.
    pub(crate) fn new(out: Arc<Outgoing>) -> Result<Feed, StereoError> {
        Ok(Feed {
            encoder: Encoder::new()?,
            out,
            gate: None,
            pcm: [0.0; FRAME * CHANNELS],
            filled: 0,
            first: None,
            spell: false,
            quiet: 0,
            opus: [0; PACKET_BYTES],
            payload: Vec::with_capacity(MAX_SOUND),
            plain: Vec::with_capacity(MAX_SOUND + 1),
            sealed: Vec::with_capacity(MAX_SOUND + 1 + session::DATA_OVERHEAD),
        })
    }

    // The capture callback: `samples` is one packet of left and right
    // interleaved, its first frame captured at `captured`, and `after_gap`
    // when something before it never came.
    pub(crate) fn hear(&mut self, samples: &[f32], captured: Instant, after_gap: bool) {
        if after_gap {
            // What came before the gap does not go out with what follows it,
            // and the next frame starts a spell from a reset encoder.
            self.filled = 0;
            self.spell = false;
            self.quiet = 0;
        }
        let gate = self.out.gate();
        if gate != self.gate {
            self.end_under(gate);
            self.gate = gate;
        }
        // A packet read before the stop was set holds only what was captured
        // before the press. One read after it may hold what came after, with
        // a stamp or without, and none of it goes.
        let Some(gate) = gate.filter(|gate| gate.stop.is_none()) else {
            return;
        };
        let route = self.out.route();
        let frames = samples.len() / CHANNELS;
        let mut at = frames_before(gate.since, captured).min(frames);
        while at < frames {
            if self.filled == 0 {
                self.first = Some(captured + frames_to_time(at));
            }
            let take = (FRAME - self.filled).min(frames - at);
            let (from, to) = (self.filled * CHANNELS, (self.filled + take) * CHANNELS);
            self.pcm[from..to].copy_from_slice(&samples[at * CHANNELS..(at + take) * CHANNELS]);
            self.filled += take;
            at += take;
            if self.filled == FRAME {
                self.frame(&route, gate.share);
            }
        }
        stats::gauge!("room.feed.payload", self.payload);
        stats::gauge!("room.feed.plain", self.plain);
        stats::gauge!("room.feed.sealed", self.sealed);
    }

    // From the room, as it lets the capture go: the pause or the switch going
    // off ends the spell under way at the press.
    pub(crate) fn end(&mut self) {
        let gate = self.out.gate();
        self.end_under(gate);
        self.gate = gate;
    }

    // The spell under way ends: while `now`, the gate as it is now, names its
    // share, the frame under way goes out cut at the stop, faded over its
    // length and silent after, as the spell's last. A share that is over
    // takes it with it.
    fn end_under(&mut self, now: Option<Gate>) {
        let ending = match (self.gate, now) {
            (Some(was), Some(now)) if self.spell && was.share == now.share => Some(now),
            _ => None,
        };
        if let Some(now) = ending {
            let kept = match (now.stop, self.first) {
                (Some(stop), Some(first)) => frames_before(stop, first).min(self.filled),
                _ => self.filled,
            };
            self.pcm[kept * CHANNELS..].fill(0.0);
            let (pairs, _) = self.pcm.as_chunks_mut::<CHANNELS>();
            for (i, pair) in pairs.iter_mut().enumerate() {
                let gain = (FRAME - 1 - i) as f32 / FRAME as f32;
                pair.iter_mut().for_each(|sample| *sample *= gain);
            }
            let route = self.out.route();
            if !route.outlets.is_empty()
                && let Ok(len) = self.encoder.encode(&self.pcm, &mut self.opus)
            {
                let first = self.first;
                self.send(&route, now.share, first, false, true, len);
            }
        }
        self.filled = 0;
        self.spell = false;
        self.quiet = 0;
    }

    fn frame(&mut self, route: &Route, share: u32) {
        self.filled = 0;
        let first = self.first;
        // Where the frame after it starts, if nothing comes to say so.
        self.first = first.map(|at| at + FRAME_TIME);
        let quiet = self.pcm.iter().all(|sample| sample.abs() < QUIET);
        let last = if quiet {
            if !self.spell {
                return;
            }
            self.quiet += 1;
            self.quiet == QUIET_FRAMES
        } else {
            self.quiet = 0;
            false
        };
        // Nobody who can hear watches: nothing is encoded, and whoever comes
        // gets a spell of their own from its start.
        if route.outlets.is_empty() {
            self.spell = false;
            self.quiet = 0;
            return;
        }
        let starts = !self.spell;
        if starts {
            let _ = self.encoder.reset();
        }
        let Ok(len) = self.encoder.encode(&self.pcm, &mut self.opus) else {
            self.spell = false;
            return;
        };
        self.spell = !last;
        if last {
            self.quiet = 0;
        }
        self.send(route, share, first, starts, last, len);
    }

    fn send(
        &mut self,
        route: &Route,
        share: u32,
        at: Option<Instant>,
        first: bool,
        last: bool,
        len: usize,
    ) {
        let out = &self.out;
        let captured = out.clock.micros(at.unwrap_or_else(Instant::now));
        let frame = SoundFrame {
            slot: 0,
            share,
            seq: out.next_seq.fetch_add(1, Ordering::Relaxed),
            captured,
            first,
            last,
            frame: &self.opus[..len],
        };
        if out.host {
            frame.write_relayed(HOST_SLOT, &mut self.payload);
        } else {
            frame.write_sent(&mut self.payload);
        }
        self.plain.clear();
        channels::frame(Channel::Sound, &self.payload, &mut self.plain);
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
        out.sent(captured);
    }
}

// How many frames from `from` on were captured before `at`: the index of the
// first one at or after it.
fn frames_before(at: Instant, from: Instant) -> usize {
    let ahead = at.saturating_duration_since(from).as_nanos();
    (ahead * u128::from(SAMPLE_RATE)).div_ceil(1_000_000_000) as usize
}

fn frames_to_time(frames: usize) -> Duration {
    Duration::from_nanos(frames as u64 * 1_000_000_000 / u64::from(SAMPLE_RATE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::Log;
    use crate::peer::Clock;
    use crate::socket::Socket;
    use crate::sound::{lock, read_sent};
    use crate::talk::Outlet;
    use crate::testing::{Wire, session_pair};
    use session::Session;

    const SHARE: u32 = 3;

    // A client's feed for share 3, its gate open from `since`, sending to one
    // link over loopback, and the far end of that link.
    struct Bench {
        feed: Feed,
        out: Arc<Outgoing>,
        socket: Arc<Socket>,
        near: Session,
        wire: Wire,
        far: Session,
        clock: Clock,
    }

    impl Bench {
        fn new(start: Instant, since: Instant) -> Bench {
            let clock = Clock::new(start);
            let socket = Arc::new(Socket::bind(0, Log::off()).expect("bind a socket"));
            let out = Arc::new(Outgoing::new(clock, false, None));
            let (near, far) = session_pair();
            out.share_changed(Some(SHARE));
            out.open(SHARE, since);
            let bench = Bench {
                feed: Feed::new(Arc::clone(&out)).unwrap(),
                out,
                socket,
                near,
                wire: Wire::new(),
                far,
                clock,
            };
            bench.watched(true);
            bench
        }

        // Someone who can hear watches, or nobody does.
        fn watched(&self, watched: bool) {
            let outlets = watched.then(|| Outlet {
                sealer: self.near.sealer().expect("the initiator may send"),
                to: self.wire.addr(),
                sent: Arc::default(),
            });
            *lock(&self.out.route) = Arc::new(Route {
                socket: Some(Arc::clone(&self.socket)),
                outlets: outlets.into_iter().collect(),
            });
        }

        // What reached the far end since the last look, as it reads it.
        fn sent(&mut self) -> Vec<(u16, u64, bool, bool)> {
            let mut plain = Vec::new();
            self.wire
                .packets()
                .iter()
                .map(|packet| {
                    self.far.decrypt(packet, &mut plain).expect("opens");
                    let (channel, payload) = channels::unframe(&plain).expect("a frame");
                    assert_eq!(channel, Channel::Sound);
                    let frame = read_sent(payload).expect("a frame Booth sends");
                    assert_eq!(frame.share, SHARE);
                    (frame.seq, frame.captured, frame.first, frame.last)
                })
                .collect()
        }
    }

    fn tone(frames: usize, from: usize) -> Vec<f32> {
        (from..from + frames)
            .flat_map(|n| {
                let left = 0.4 * (n as f32 * 0.07).sin();
                [left, -left]
            })
            .collect()
    }

    // Packets of 128 frames make 480-frame frames, each stamped with its
    // first sample's time: the packet's, plus that sample's place counted in
    // frames of left and right, not in samples.
    #[test]
    fn frames_are_cut_by_sample_count_and_timed_by_their_first_frame() {
        let start = Instant::now();
        let mut bench = Bench::new(start, start);
        for packet in 0..30 {
            let at = start + frames_to_time(packet * 128);
            bench.feed.hear(&tone(128, packet * 128), at, packet == 0);
        }
        let sent = bench.sent();
        assert_eq!(sent.len(), 30 * 128 / FRAME);
        let base = bench.clock.micros(start);
        for (k, &(seq, captured, first, last)) in sent.iter().enumerate() {
            let expected = base + frames_to_time(k * FRAME).as_micros() as u64;
            assert!(
                captured.abs_diff(expected) <= 1,
                "frame {k}: {captured} {expected}"
            );
            assert_eq!(seq, sent[0].0.wrapping_add(k as u16));
            assert_eq!((first, last), (k == 0, false), "frame {k}");
        }
    }

    // 200 ms of silence ends a spell, its 20th frame with LAST, and nothing
    // goes out until a louder frame starts the next from a reset encoder.
    #[test]
    fn silence_ends_a_spell_and_sound_starts_the_next() {
        let start = Instant::now();
        let mut bench = Bench::new(start, start);
        let mut at = start;
        let mut hear = |bench: &mut Bench, samples: Vec<f32>| {
            bench.feed.hear(&samples, at, false);
            at += frames_to_time(samples.len() / CHANNELS);
        };
        hear(&mut bench, tone(FRAME * 5, 0));
        hear(&mut bench, vec![0.0; FRAME * CHANNELS * 30]);
        let spell = bench.sent();
        assert_eq!(spell.len(), 5 + QUIET_FRAMES as usize);
        assert!(spell[0].2 && !spell[0].3);
        assert!(spell[1..spell.len() - 1].iter().all(|f| !f.2 && !f.3));
        assert!(spell.last().unwrap().3, "the 20th quiet frame says LAST");
        // A dither of one step at 16 bits is still silence.
        hear(&mut bench, vec![1.0 / 32_768.0; FRAME * CHANNELS * 30]);
        assert!(bench.sent().is_empty());
        hear(&mut bench, tone(FRAME * 3, 0));
        let next = bench.sent();
        assert_eq!(next.len(), 3);
        assert!(next[0].2 && !next[0].3);
        assert_eq!(next[0].0, spell.last().unwrap().0.wrapping_add(1));
    }

    // Nothing captured before the switch went on goes out, and the first
    // frame starts at the first sample at or after the press. At the pause,
    // the room ends the frame under way at the press, faded, with LAST, and
    // nothing of a packet read after it goes out, whatever its time says.
    #[test]
    fn the_gate_holds_to_the_presses() {
        let start = Instant::now();
        let since = start + frames_to_time(200) + Duration::from_nanos(1);
        let mut bench = Bench::new(start, since);
        bench.feed.hear(&tone(FRAME * 2, 0), start, true);
        let sent = bench.sent();
        assert_eq!(sent.len(), 1, "960 frames less the 201 before the press");
        let expected = bench.clock.micros(start + frames_to_time(201));
        assert!(sent[0].1.abs_diff(expected) <= 1, "{sent:?}");
        assert!(sent[0].2);

        let press = start + frames_to_time(FRAME * 2 + 100);
        bench.out.set_held(true, press);
        bench.feed.end();
        let ended = bench.sent();
        assert_eq!(ended.len(), 1);
        assert!(ended[0].3 && !ended[0].2, "{ended:?}");
        // Read after the press, stamped before it or with no stamp at all.
        bench.feed.hear(
            &tone(FRAME * 4, 0),
            start + frames_to_time(FRAME * 2),
            false,
        );
        assert!(bench.sent().is_empty());

        // Resumed: a spell of its own, from a reset encoder.
        let resumed = press + Duration::from_millis(50);
        bench.out.set_held(false, resumed);
        bench.feed.hear(&tone(FRAME * 2, 0), resumed, false);
        let again = bench.sent();
        assert_eq!(again.len(), 2);
        assert!(again[0].2 && !again[1].2);
    }

    // Stop sharing's stop holds whatever the switch does before the room
    // takes it in: Share sound turned on again opens nothing.
    #[test]
    fn stop_sharing_holds_against_the_switch() {
        let start = Instant::now();
        let bench = Bench::new(start, start);
        bench.out.end_share(start);
        bench.out.open(SHARE, start + Duration::from_millis(1));
        assert!(bench.out.shut());
    }

    // A gap drops the part of a frame from before it, and the frame after it
    // starts a spell; while nobody who can hear watches nothing is encoded,
    // and the first frame once someone can starts one too.
    #[test]
    fn a_gap_or_nobody_to_hear_starts_the_next_spell() {
        let start = Instant::now();
        let mut bench = Bench::new(start, start);
        bench.feed.hear(&tone(FRAME + 100, 0), start, true);
        let later = start + Duration::from_secs(2);
        bench.feed.hear(&tone(FRAME, 0), later, true);
        let sent = bench.sent();
        assert_eq!(sent.len(), 2);
        assert!(sent[0].2 && sent[1].2, "{sent:?}");
        assert!(sent[1].1.abs_diff(bench.clock.micros(later)) <= 1);

        bench.watched(false);
        let mut at = later + frames_to_time(FRAME);
        bench.feed.hear(&tone(FRAME * 3, 0), at, false);
        assert!(bench.sent().is_empty());
        bench.watched(true);
        at += frames_to_time(FRAME * 3);
        bench.feed.hear(&tone(FRAME * 2, 0), at, false);
        let back = bench.sent();
        assert_eq!(back.len(), 2);
        assert!(back[0].2 && !back[1].2);
    }

    // After a rest nothing goes out, though the capture runs on, until the
    // share's own capture answers: then a spell of its own, from the answer.
    // A share that is over says nothing about the next.
    #[test]
    fn a_rest_holds_the_sound_until_the_capture_answers() {
        let start = Instant::now();
        let mut bench = Bench::new(start, start);
        bench.out.set_unseen(SHARE, true, start);
        assert!(!bench.out.paused(), "the capture was let go");
        bench.feed.hear(&tone(FRAME * 3, 0), start, true);
        assert!(bench.sent().is_empty());
        let other = start + frames_to_time(FRAME * 3);
        bench.out.set_unseen(SHARE + 1, false, other);
        bench.feed.hear(&tone(FRAME * 3, 0), other, false);
        assert!(bench.sent().is_empty(), "another share's answer opened it");
        let next = other + frames_to_time(FRAME * 3);
        let seen = next + FRAME_TIME;
        bench.out.set_unseen(SHARE, false, seen);
        bench.feed.hear(&tone(FRAME * 4, 0), next, false);
        let back = bench.sent();
        assert_eq!(back.len(), 3, "{back:?}");
        assert!(back[0].2 && !back[1].2, "{back:?}");
        assert!(
            back[0].1.abs_diff(bench.clock.micros(seen)) <= 1,
            "{back:?}"
        );
    }
}
