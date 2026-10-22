// PC sound through rooms on loopback, with the fake audio devices: no test
// here opens a real microphone or plays a sound. Booth sends PC sound from a
// share's own capture, which is not here yet, so the sharer is a friend's
// program played by hand (common::Hand) sending what a sharer's PC sends.

mod common;

use std::f64::consts::TAU;
use std::thread;
use std::time::{Duration, Instant};

use channels::Channel;
use common::voiced::{RATE, Voiced, alone, silence, voiced};
use common::{Hand, code_to_invite, loopback, timers, wall_clock_us};
use invite::Version;
use room::view::View;
use room::{Config, TalkMode};
use voice::audio::fake::Fake;
use voice::stereo::{Encoder, FRAME, MAX_PACKET};

const WAIT: Duration = Duration::from_secs(3);
const PC_SOUND_SINCE: Version = Version {
    major: 1,
    minor: 3,
    patch: 0,
};
const OLDER: Version = Version {
    major: 1,
    minor: 2,
    patch: 0,
};

fn member(name: &str, version: Version) -> (Config, Fake, Fake) {
    let mut config = voiced(name, timers(), silence, TalkMode::PushToTalk, true);
    config.0.version = version;
    config
}

// 10 ms stereo frames as a sharer's encoder makes them: 500 Hz at 0.4 on
// the left, 700 Hz at 0.2 on the right.
fn stereo_frames(count: usize) -> Vec<Vec<u8>> {
    let mut encoder = Encoder::new().expect("a stereo encoder");
    (0..count)
        .map(|i| {
            let pcm: Vec<f32> = (0..FRAME)
                .flat_map(|n| {
                    let t = (i * FRAME + n) as f64 / RATE as f64;
                    [
                        (0.4 * (TAU * 500.0 * t).sin()) as f32,
                        (0.2 * (TAU * 700.0 * t).sin()) as f32,
                    ]
                })
                .collect();
            let mut out = [0u8; MAX_PACKET];
            let len = encoder.encode(&pcm, &mut out).expect("encoded");
            out[..len].to_vec()
        })
        .collect()
}

// A sharer's PC sound packet, as the room's sound\wire.rs lays it out: kind
// 1 (sent to the host) and slot 0, the share, the sequence number, the
// capture time on the sharer's ping clock, the flags (FIRST is 2), the
// frame's length and the frame.
fn sent(share: u32, seq: u16, captured: u64, first: bool, frame: &[u8]) -> Vec<u8> {
    let mut out = vec![1, 0];
    out.extend_from_slice(&share.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&captured.to_le_bytes());
    out.push(if first { 2 } else { 0 });
    out.push(frame.len() as u8);
    out.extend_from_slice(frame);
    out
}

// ShareSound, kind 28: whether the share's PC sound goes out.
fn share_sound(share: u32, on: bool) -> Vec<u8> {
    let mut out = vec![28];
    out.extend_from_slice(&share.to_le_bytes());
    out.push(u8::from(on));
    out
}

// The amplitude of a sine at `hz` in `samples`, which should hold a whole
// number of its cycles.
fn amplitude(samples: &[f32], hz: f64) -> f64 {
    let (mut re, mut im) = (0.0, 0.0);
    for (n, &sample) in samples.iter().enumerate() {
        let phase = TAU * hz * n as f64 / RATE as f64;
        re += f64::from(sample) * phase.cos();
        im += f64::from(sample) * phase.sin();
    }
    2.0 * (re * re + im * im).sqrt() / samples.len() as f64
}

// What the two sides of `speakers` played over the 200 ms, whole cycles of
// both tones, that held the most of the left tone, from 100 ms after they
// first played anything: a busy PC's late frame elsewhere is no matter.
fn sides(speakers: &Fake) -> Option<(Vec<f32>, Vec<f32>)> {
    const WINDOW: usize = 9_600;
    let written = speakers.record().written;
    let (frames, _) = written.as_chunks::<2>();
    let first = frames.iter().position(|[left, _]| left.abs() > 0.05)? + 4_800;
    let split = |at: usize| -> (Vec<f32>, Vec<f32>) {
        frames[at..at + WINDOW]
            .iter()
            .map(|&[left, right]| (left, right))
            .unzip()
    };
    (first..frames.len().checked_sub(WINDOW)?)
        .step_by(2_400)
        .map(split)
        .max_by(|(a, _), (b, _)| amplitude(a, 500.0).total_cmp(&amplitude(b, 500.0)))
}

fn loudest(speakers: &Fake) -> f32 {
    speakers
        .record()
        .written
        .iter()
        .fold(0.0, |most, sample| most.max(sample.abs()))
}

// Bo, a friend, shares with his PC sound on. The host passes it on to Ana,
// who watches on 1.3.0, and plays it itself as it watches too: each hears
// Bo's left on the left and his right on the right, at half the level he
// sent, with a capture to ear time through the host's word on Bo's clock.
// Cy watches on 1.2.0 and Dee does not watch: neither hears any of it, and
// Cy counts nothing bad. Bo's word that it is off ends Ana's ear.
#[test]
fn a_friends_pc_sound_reaches_its_watchers_on_their_sides() {
    let _alone = alone();
    let host = Voiced::host_with(member("Mara", PC_SOUND_SINCE));
    host.room().new_invite(true);
    let code = host
        .member
        .wait_for(WAIT, "a multi-use invite", |v| {
            v.invite
                .as_ref()
                .is_some_and(|i| i.multi_use && !i.code.is_empty())
        })
        .invite
        .expect("an invite")
        .code;
    let to_host = loopback(host.member.port());
    let ana = Voiced::join(
        member("Ana", PC_SOUND_SINCE),
        code_to_invite(&code, to_host),
    );
    let cy = Voiced::join(member("Cy", OLDER), code_to_invite(&code, to_host));
    let dee = Voiced::join(
        member("Dee", PC_SOUND_SINCE),
        code_to_invite(&code, to_host),
    );
    let mut bo = Hand::join(&code_to_invite(&code, to_host), to_host, "Bo");
    // ShareStart, kind 9, at 60 fps.
    bo.say(&[9, 60]);
    let share = ana
        .member
        .wait_for(WAIT, "Bo's share", |v| {
            v.people.len() == 5 && v.share.current.is_some()
        })
        .share
        .current
        .expect("a share")
        .number;
    for watcher in [&host, &ana, &cy] {
        watcher.room().watch(share, true);
    }
    host.member.wait_for(WAIT, "three watching", |v| {
        v.share
            .current
            .as_ref()
            .is_some_and(|current| current.watchers == Some(3))
    });
    bo.say(&share_sound(share, true));

    // 1.5 s of it at a sharer's pace, the host's pings answered as they
    // come. Those that waited while Bo answered nothing would put his clock
    // ahead by half their wait, so they go unanswered.
    let frames = stereo_frames(150);
    bo.received();
    let start = Instant::now();
    for (seq, frame) in frames.iter().enumerate() {
        let due = start + Duration::from_millis(10 * seq as u64);
        while Instant::now() < due {
            bo.answer_pings();
            thread::sleep(Duration::from_millis(1));
        }
        let packet = sent(share, seq as u16, wall_clock_us(), seq == 0, frame);
        bo.send(Channel::Sound, &packet);
    }
    let heard = |view: &View| {
        view.share.watched_sound
            && view
                .numbers
                .sound
                .is_some_and(|sound| sound.capture_to_ear.is_some())
    };
    let ana_view = ana.member.wait_for(WAIT, "Ana's pc sound numbers", heard);
    let host_view = host
        .member
        .wait_for(WAIT, "the host's pc sound numbers", heard);

    for (who, view, speakers) in [
        ("Ana", &ana_view, &ana.speakers),
        ("the host", &host_view, &host.speakers),
    ] {
        let numbers = view.numbers.sound.expect("pc sound numbers");
        let capture_to_ear = numbers.capture_to_ear.expect("a capture to ear time");
        let (left, right) = sides(speakers).expect("pc sound on the speakers");
        let (left_500, left_700) = (amplitude(&left, 500.0), amplitude(&left, 700.0));
        let (right_500, right_700) = (amplitude(&right, 500.0), amplitude(&right, 700.0));
        println!(
            "{who}: capture to ear {:.1} ms, p95 {:.1} ms{}, buffer {} ms, {} made up, loss {:.1}%; left 500 Hz {left_500:.3} 700 Hz {left_700:.4}, right 700 Hz {right_700:.3} 500 Hz {right_500:.4}",
            capture_to_ear.median_ms,
            capture_to_ear.p95_ms,
            if capture_to_ear.about { " (about)" } else { "" },
            numbers.depth_ms,
            numbers.made_up,
            numbers.loss_pct,
        );
        // A buffer at its 60 ms cap, the speakers' latency and the codec's
        // lookahead stay under this.
        assert!(
            (0.0..100.0).contains(&capture_to_ear.median_ms),
            "{who}: {capture_to_ear:?}"
        );
        assert!((0.17..0.23).contains(&left_500), "{who}: {left_500}");
        assert!((0.085..0.115).contains(&right_700), "{who}: {right_700}");
        assert!(left_700 < 0.01 && right_500 < 0.01, "{who}: the sides mix");
        assert_eq!(view.numbers.dropped_bad, 0, "{who}");
    }
    for (who, member) in [("Cy", &cy), ("Dee", &dee)] {
        let view = member.view();
        assert!(view.numbers.sound.is_none(), "{who}");
        assert!(!view.share.watched_sound, "{who}");
        assert_eq!(view.numbers.dropped_bad, 0, "{who}");
        assert!(loudest(&member.speakers) < 1e-3, "{who} heard it");
    }

    bo.say(&share_sound(share, false));
    ana.member.wait_for(WAIT, "Ana's ear gone", |v| {
        !v.share.watched_sound && v.numbers.sound.is_none()
    });
}
