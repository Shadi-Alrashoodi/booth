// PC sound through rooms on loopback, with the fake audio devices: no test
// here opens a real microphone, plays a sound or captures what this PC
// plays. The sharer is Booth itself, its PC sound a fake that plays a tone,
// except in the first test, where a friend's program played by hand
// (common::Hand) sends what a sharer's PC sends. Where a test reads every
// packet the host passes on, the watcher is played by hand too.

mod common;

use std::f64::consts::TAU;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use capture::PauseReason;
use channels::Channel;
use common::voiced::{RATE, Voiced, alone, silence, sine, talking, voiced};
use common::window::Window;
use common::{Hand, code_to_invite, loopback, poll, timers, wall_clock_us};
use invite::Version;
use room::view::{LineKind, NoSound, OwnShare, View};
use room::{Config, Devices, Show, TalkMode, VideoConfig, VideoSource};
use voice::audio::AudioError;
use voice::audio::fake::{Fake, Setup};
use voice::stereo::{Decoder, Encoder, FRAME, MAX_PACKET};

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

// The sharer's PC sound: 500 Hz at 0.4 on the left, 700 Hz at 0.2 on the
// right.
fn left_and_right(frame: u64, channel: u16) -> f32 {
    if channel == 0 {
        sine(frame, 500.0, 0.4)
    } else {
        sine(frame, 700.0, 0.2)
    }
}

// The sharer's voice, which plays in the middle: 300 Hz at 0.1.
fn voice_300(frame: u64, _: u16) -> f32 {
    sine(frame, 300.0, 0.1)
}

// PC sound playing `signal` in 128-frame packets, each stop taking `close`.
fn pc_sound(signal: fn(u64, u16) -> f32, close: Duration) -> Fake {
    let setup = Setup {
        signal,
        close_time: close,
        ..Setup::default()
    };
    Fake::new(setup, &[("pc", "Test PC sound")], Some("pc"))
}

// A member whose share sends `pc_sound`, its microphone playing
// `microphone`.
fn sharer(
    name: &str,
    version: Version,
    microphone: fn(u64, u16) -> f32,
    pc_sound: &Fake,
) -> (Config, Fake, Fake) {
    let mut config = voiced(name, timers(), microphone, TalkMode::PushToTalk, true);
    config.0.version = version;
    if let Devices::Fake { pc_sound: fake, .. } = &mut config.0.voice.devices {
        *fake = pc_sound.clone();
    }
    config
}

// The host, a multi-use invite to it, and where it is.
fn hosting(config: (Config, Fake, Fake)) -> (Voiced, String, SocketAddr) {
    let host = Voiced::host_with(config);
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
    (host, code, to_host)
}

fn joins(config: (Config, Fake, Fake), code: &str, to_host: SocketAddr, people: usize) -> Voiced {
    let member = Voiced::join(config, code_to_invite(code, to_host));
    member
        .member
        .wait_for(WAIT, "in the room", |v| v.people.len() == people);
    member
}

fn own_share(view: &View) -> Option<u32> {
    match view.share.own {
        OwnShare::Sharing { number, .. } => Some(number),
        _ => None,
    }
}

fn shares(member: &Voiced) -> u32 {
    member.room().share(60, None);
    let view = member
        .member
        .wait_for(WAIT, "the share granted", |v| own_share(v).is_some());
    own_share(&view).expect("granted")
}

fn watches(watcher: &Voiced, share: u32) {
    watcher
        .member
        .wait_for(WAIT, "the share in the roster", |v| {
            v.share
                .current
                .as_ref()
                .is_some_and(|current| current.number == share)
        });
    watcher.room().watch(share, true);
}

fn watched_by(sharer: &Voiced, count: u8) {
    sharer.member.wait_for(WAIT, "the watchers counted", |v| {
        v.share
            .current
            .as_ref()
            .is_some_and(|current| current.watchers == Some(count))
    });
}

// A watcher played by hand on 1.3.0, which keeps every packet the host
// passes on.
fn hand_watcher(code: &str, to_host: SocketAddr) -> Hand {
    Hand::join_as(
        &code_to_invite(code, to_host),
        to_host,
        "Ana",
        PC_SOUND_SINCE,
    )
}

// Watch, kind 12: the share, on, and flags that take no HEVC.
fn watch(share: u32) -> Vec<u8> {
    let mut out = vec![12];
    out.extend_from_slice(&share.to_le_bytes());
    out.extend_from_slice(&[1, 0]);
    out
}

// Watch, kind 12, on, from a watcher that takes HEVC: the facts that count
// it never move the share to H.264, so the host can hold them back.
fn watch_taking_hevc(share: u32) -> Vec<u8> {
    let mut out = vec![12];
    out.extend_from_slice(&share.to_le_bytes());
    out.extend_from_slice(&[1, 1]);
    out
}

// WatchLimits, kind 25, as a viewer with a cap sends after its Watch: no
// 4K, no height or frame rate cap, and `kbps`, one of the caps offered.
fn watch_limits(share: u32, kbps: u32) -> Vec<u8> {
    let mut out = vec![25];
    out.extend_from_slice(&share.to_le_bytes());
    out.push(0);
    out.extend_from_slice(&0u16.to_le_bytes());
    out.push(0);
    out.extend_from_slice(&kbps.to_le_bytes());
    out
}

// Watch, kind 12, off.
fn stop_watching(share: u32) -> Vec<u8> {
    let mut out = vec![12];
    out.extend_from_slice(&share.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out
}

// A PC sound packet as the host passed it on, at the offsets sound\wire.rs
// gives its fields.
struct Passed {
    share: u32,
    seq: u16,
    captured: u64,
    first: bool,
    last: bool,
    frame: Vec<u8>,
}

fn passed(payload: &[u8]) -> Passed {
    let len = usize::from(payload[17]);
    Passed {
        share: u32::from_le_bytes([payload[2], payload[3], payload[4], payload[5]]),
        seq: u16::from_le_bytes([payload[6], payload[7]]),
        captured: u64::from_le_bytes(payload[8..16].try_into().expect("8 bytes")),
        first: payload[16] & 2 != 0,
        last: payload[16] & 1 != 0,
        frame: payload[18..18 + len].to_vec(),
    }
}

// The flags of each, for a failure to show.
fn flags(sound: &[Passed]) -> Vec<(u16, bool, bool)> {
    sound
        .iter()
        .map(|packet| (packet.seq, packet.first, packet.last))
        .collect()
}

// What came to the hand over `span`, its pings answered as they came: the
// PC sound passed on, and each ShareSound it was told.
fn listen(hand: &mut Hand, span: Duration) -> (Vec<Passed>, Vec<(u32, bool)>) {
    let mut sound = Vec::new();
    let mut told = Vec::new();
    let end = Instant::now() + span;
    while Instant::now() < end {
        hand.answer_pings();
        for (channel, payload) in hand.received() {
            if channel == Channel::Sound {
                sound.push(passed(&payload));
            }
        }
        for message in hand.messages() {
            if let [28, a, b, c, d, on] = message[..] {
                told.push((u32::from_le_bytes([a, b, c, d]), on == 1));
            }
        }
        thread::sleep(Duration::from_millis(1));
    }
    (sound, told)
}

// The most of `hz` on the left in any frame of `sound`, decoded as a
// watcher decodes it, each spell from its first frame.
fn most_in_frames(sound: &[Passed], hz: f64) -> f64 {
    let mut decoder = Decoder::new().expect("a stereo decoder");
    let mut out = [0f32; FRAME * 2];
    let mut most = 0.0f64;
    for packet in sound {
        if packet.first {
            decoder.reset().expect("reset");
        }
        let frames = decoder.decode(&packet.frame, &mut out).expect("decoded");
        let left: Vec<f32> = out[..frames * 2].iter().step_by(2).copied().collect();
        most = most.max(amplitude(&left, hz));
    }
    most
}

// The most of `hz` on one side in any 200 ms of what `speakers` played.
fn most_played(speakers: &Fake, side: usize, hz: f64) -> f64 {
    let written = speakers.record().written;
    let (frames, _) = written.as_chunks::<2>();
    let played: Vec<f32> = frames.iter().map(|pair| pair[side]).collect();
    let (blocks, _) = played.as_chunks::<9_600>();
    blocks
        .iter()
        .map(|block| amplitude(block, hz))
        .fold(0.0, f64::max)
}

// Mara shares with Share sound on and talks. Ana, watching on 1.3.0, hears
// its left on her left and its right on her right, at half the level it was
// captured, with Mara's voice in the middle of both. Cy, watching on 1.2.0,
// and Dee, who does not watch, hear her voice and none of the PC sound, and
// count nothing bad.
#[test]
fn the_hosts_pc_sound_reaches_its_watchers_on_their_sides() {
    let _alone = alone();
    let fake = pc_sound(left_and_right, Duration::ZERO);
    let (host, code, to_host) = hosting(sharer("Mara", PC_SOUND_SINCE, voice_300, &fake));
    let ana = joins(member("Ana", PC_SOUND_SINCE), &code, to_host, 2);
    let cy = joins(member("Cy", OLDER), &code, to_host, 3);
    let dee = joins(member("Dee", PC_SOUND_SINCE), &code, to_host, 4);
    let share = shares(&host);
    watches(&ana, share);
    watches(&cy, share);
    watched_by(&host, 2);
    host.room().talk(true);
    for listener in [&ana, &cy, &dee] {
        listener
            .member
            .wait_for(WAIT, "Mara talking", |v| talking(v, "Mara"));
    }
    host.room().share_sound(true);
    assert!(host.view().share.own_sound.on);
    let view = ana.member.wait_for(WAIT, "Ana hearing the pc sound", |v| {
        v.share.watched_sound
            && v.numbers
                .sound
                .is_some_and(|sound| sound.capture_to_ear.is_some())
    });
    thread::sleep(Duration::from_millis(600));

    let numbers = view.numbers.sound.expect("pc sound numbers");
    let capture_to_ear = numbers.capture_to_ear.expect("a capture to ear time");
    let (left, right) = sides(&ana.speakers).expect("pc sound on the speakers");
    let at = |side: &[f32], hz| amplitude(side, hz);
    println!(
        "Ana: capture to ear {:.1} ms, p95 {:.1} ms, buffer {} ms, {} made up; left 500 Hz {:.3} 700 Hz {:.4} voice {:.3}, right 700 Hz {:.3} 500 Hz {:.4} voice {:.3}",
        capture_to_ear.median_ms,
        capture_to_ear.p95_ms,
        numbers.depth_ms,
        numbers.made_up,
        at(&left, 500.0),
        at(&left, 700.0),
        at(&left, 300.0),
        at(&right, 700.0),
        at(&right, 500.0),
        at(&right, 300.0),
    );
    assert!(
        (0.0..100.0).contains(&capture_to_ear.median_ms),
        "{capture_to_ear:?}"
    );
    assert!((0.17..0.23).contains(&at(&left, 500.0)));
    assert!((0.085..0.115).contains(&at(&right, 700.0)));
    assert!(
        at(&left, 700.0) < 0.01 && at(&right, 500.0) < 0.01,
        "the sides mix"
    );
    for side in [&left, &right] {
        assert!(
            (0.07..0.13).contains(&at(side, 300.0)),
            "the voice is not in the middle"
        );
    }
    assert_eq!(ana.view().numbers.dropped_bad, 0);
    for (who, member) in [("Cy", &cy), ("Dee", &dee)] {
        let view = member.view();
        assert!(view.numbers.sound.is_none(), "{who}");
        assert!(!view.share.watched_sound, "{who}");
        assert_eq!(view.numbers.dropped_bad, 0, "{who}");
        let pc_sound =
            most_played(&member.speakers, 0, 500.0).max(most_played(&member.speakers, 1, 700.0));
        assert!(pc_sound < 0.005, "{who} heard the pc sound: {pc_sound}");
        assert!(
            most_played(&member.speakers, 0, 300.0) > 0.07,
            "{who} heard no voice"
        );
    }
}

// Bo, a friend, shares with Share sound on. The host passes it on to Ana and
// plays it itself, both watching on 1.3.0: each hears his left on the left
// and his right on the right.
#[test]
fn a_friends_pc_sound_goes_through_the_host() {
    let _alone = alone();
    let fake = pc_sound(left_and_right, Duration::ZERO);
    let (host, code, to_host) = hosting(member("Mara", PC_SOUND_SINCE));
    let ana = joins(member("Ana", PC_SOUND_SINCE), &code, to_host, 2);
    let bo = joins(
        sharer("Bo", PC_SOUND_SINCE, silence, &fake),
        &code,
        to_host,
        3,
    );
    let share = shares(&bo);
    watches(&host, share);
    watches(&ana, share);
    watched_by(&bo, 2);
    bo.room().share_sound(true);
    let own = bo.view().share.own_sound;
    assert!(own.on && own.unavailable.is_none(), "{own:?}");
    for (who, member) in [("Ana", &ana), ("the host", &host)] {
        let view = member.member.wait_for(WAIT, "the pc sound heard", |v| {
            v.share.watched_sound && v.numbers.sound.is_some()
        });
        thread::sleep(Duration::from_millis(600));
        let (left, right) = sides(&member.speakers).expect("pc sound on the speakers");
        let (left_500, right_700) = (amplitude(&left, 500.0), amplitude(&right, 700.0));
        println!("{who}: left 500 Hz {left_500:.3}, right 700 Hz {right_700:.3}");
        assert!((0.17..0.23).contains(&left_500), "{who}: {left_500}");
        assert!((0.085..0.115).contains(&right_700), "{who}: {right_700}");
        assert!(amplitude(&left, 700.0) < 0.01 && amplitude(&right, 500.0) < 0.01);
        assert_eq!(view.numbers.dropped_bad, 0, "{who}");
    }
}

// Bo on 1.3.0 shares through a host on 1.2.0, which would count a packet on
// channel 8 or a ShareSound as bad. His Share sound stays off and says why,
// his PC sound never opens, and the host counts nothing bad.
#[test]
fn a_client_sends_no_pc_sound_through_a_host_on_1_2_0() {
    let _alone = alone();
    let fake = pc_sound(left_and_right, Duration::ZERO);
    let (host, code, to_host) = hosting(member("Mara", OLDER));
    let ana = joins(member("Ana", PC_SOUND_SINCE), &code, to_host, 2);
    let bo = joins(
        sharer("Bo", PC_SOUND_SINCE, silence, &fake),
        &code,
        to_host,
        3,
    );
    let share = shares(&bo);
    watches(&ana, share);
    watched_by(&host, 1);
    bo.room().share_sound(true);
    let own = bo.view().share.own_sound;
    assert!(!own.on);
    assert_eq!(own.unavailable, Some(NoSound::OldHost));
    assert_eq!(
        NoSound::OldHost.sentence(),
        "The host needs Booth 1.3.0 or later for sound."
    );
    thread::sleep(Duration::from_millis(500));
    assert_eq!(host.view().numbers.dropped_bad, 0);
    assert!(fake.record().opens.is_empty(), "the pc sound opened");
    assert!(ana.view().numbers.sound.is_none());
}

// A watcher played by hand watches Mara's share with Share sound on: the
// sound and the word that it is on come, each frame of it named for the
// share.
fn hand_watches(fake: &Fake) -> (Voiced, Hand, u32, Vec<Passed>) {
    let (host, code, to_host) = hosting(sharer("Mara", PC_SOUND_SINCE, silence, fake));
    let mut ana = hand_watcher(&code, to_host);
    let share = shares(&host);
    ana.say(&watch(share));
    watched_by(&host, 1);
    host.room().share_sound(true);
    let (sound, told) = listen(&mut ana, Duration::from_millis(400));
    assert!(sound.len() >= 25, "{}", sound.len());
    assert!(sound[0].first, "{:?}", flags(&sound));
    assert!(
        sound
            .iter()
            .all(|packet| packet.share == share && !packet.last)
    );
    assert_eq!(told, [(share, true)]);
    (host, ana, share, sound)
}

// Paused by hand, the share's PC sound ends with the frame under way,
// marked LAST, and nothing comes after it, nor a word that it is off: the
// share says it is paused. Resumed, it comes back from a FIRST.
#[test]
fn a_pause_ends_the_sound_and_resume_brings_it_back() {
    let _alone = alone();
    let fake = pc_sound(left_and_right, Duration::ZERO);
    let (host, mut ana, _, _) = hand_watches(&fake);
    host.room().pause_share(true);
    let (paused, told) = listen(&mut ana, Duration::from_millis(400));
    let last = paused.iter().position(|packet| packet.last);
    assert_eq!(last, Some(paused.len() - 1), "{:?}", flags(&paused));
    assert!(told.is_empty(), "{told:?}");
    assert!(host.view().share.own_sound.on);
    host.room().pause_share(false);
    let (resumed, told) = listen(&mut ana, Duration::from_millis(400));
    assert!(resumed.len() >= 25, "{}", resumed.len());
    assert!(resumed[0].first, "{:?}", flags(&resumed));
    assert!(told.is_empty(), "{told:?}");
}

static MARK: AtomicBool = AtomicBool::new(false);

// The two tones, or 1 kHz on both sides while MARK is set.
fn marked(frame: u64, channel: u16) -> f32 {
    if MARK.load(Ordering::Relaxed) {
        sine(frame, 1000.0, 0.5)
    } else {
        left_and_right(frame, channel)
    }
}

// Paused, then resumed 50 ms later, on PC sound whose stop takes a second:
// the capture let go at the pause is still closing when the one for the
// resume opens. Nothing captured between the two presses, a 1 kHz mark,
// reaches the watcher.
#[test]
fn nothing_captured_between_a_pause_and_a_quick_resume_goes_out() {
    let _alone = alone();
    let fake = pc_sound(marked, Duration::from_secs(1));
    let (host, mut ana, _, mut sound) = hand_watches(&fake);
    host.room().pause_share(true);
    MARK.store(true, Ordering::Relaxed);
    let (between, _) = listen(&mut ana, Duration::from_millis(50));
    MARK.store(false, Ordering::Relaxed);
    host.room().pause_share(false);
    let (after, _) = listen(&mut ana, Duration::from_millis(500));
    assert!(after.len() >= 25 && after[0].first, "{:?}", flags(&after));
    sound.extend(between);
    sound.extend(after);
    let (tone, mark) = (
        most_in_frames(&sound, 500.0),
        most_in_frames(&sound, 1000.0),
    );
    let record = fake.record();
    println!(
        "500 Hz {tone:.3}, the mark {mark:.4}, {} captures open at most at once",
        record.most_at_once
    );
    assert!(tone > 0.3, "{tone}");
    assert!(
        mark < 0.1,
        "sound from between the presses went out: {mark}"
    );
    assert!(
        record.most_at_once >= 2,
        "the first capture had closed before the second opened"
    );
}

// After 2 s in which the PC sound sent nothing, the share is paused and
// packets with no stamp come: nothing of them goes out, only the frame that
// was under way at the press, with LAST.
#[test]
fn a_packet_without_a_stamp_after_a_gap_sends_nothing_after_the_press() {
    let _alone = alone();
    let fake = pc_sound(left_and_right, Duration::ZERO);
    let (host, mut ana, _, _) = hand_watches(&fake);
    fake.stall(true);
    listen(&mut ana, Duration::from_secs(2));
    fake.unstamped(true);
    host.room().pause_share(true);
    fake.stall(false);
    let (after, told) = listen(&mut ana, Duration::from_millis(500));
    assert!(after.len() == 1 && after[0].last, "{:?}", flags(&after));
    assert!(told.is_empty(), "{told:?}");
    fake.unstamped(false);
    host.room().pause_share(false);
    let (back, _) = listen(&mut ana, Duration::from_millis(400));
    assert!(back.len() >= 25 && back[0].first, "{:?}", flags(&back));
}

// Mara's share rests with Share sound on, since nobody watches, and her PC
// goes to the lock screen meanwhile, which a share at rest does not see. Ana
// starts watching: no PC sound comes before the share's capture has looked,
// nor once it finds the secure desktop, which lets the capture go. With the
// picture back it comes, from a FIRST. The share's thread is played by hand.
#[test]
fn a_new_watcher_hears_nothing_until_the_share_has_seen_the_desktop() {
    let _alone = alone();
    let fake = pc_sound(left_and_right, Duration::ZERO);
    let (host, code, to_host) = hosting(sharer("Mara", PC_SOUND_SINCE, silence, &fake));
    let mut ana = hand_watcher(&code, to_host);
    let share = shares(&host);
    let sharing = host.room().sharing();
    sharing.set_unseen(share, true);
    host.room().share_sound(true);
    captures_open(&fake, 1, 1);
    ana.say(&watch(share));
    watched_by(&host, 1);
    let (unseen, told) = listen(&mut ana, Duration::from_millis(400));
    assert!(unseen.is_empty(), "{:?}", flags(&unseen));
    assert_eq!(told, [(share, true)]);
    sharing.set_away(share, true);
    captures_open(&fake, 1, 0);
    let (locked, _) = listen(&mut ana, Duration::from_millis(300));
    assert!(locked.is_empty(), "{:?}", flags(&locked));
    // The first picture after the secure desktop, and so the capture's
    // first answer since the rest.
    sharing.set_away(share, false);
    sharing.set_unseen(share, false);
    let (back, told) = listen(&mut ana, Duration::from_millis(400));
    assert!(back.len() >= 25 && back[0].first, "{:?}", flags(&back));
    assert!(told.is_empty(), "{told:?}");
}

// What a window share's looks at the desktop find, from the test, until it
// is dropped and they ask Windows again.
struct Looks;

impl Looks {
    fn find(away: Option<PauseReason>) -> Looks {
        Looks::set(away);
        Looks
    }

    fn set(away: Option<PauseReason>) {
        capture::fault::set_desktop(away, Duration::ZERO);
    }
}

impl Drop for Looks {
    fn drop(&mut self) {
        capture::fault::clear();
    }
}

// Mara hosts and shares a window with Share sound on, or Bo, a friend, does;
// Ana, played by hand, watches it and hears it.
struct WindowHeard {
    bo: Option<Voiced>,
    host: Voiced,
    ana: Hand,
    share: u32,
    _window: Window,
}

// None where no monitor is attached, as on the runner.
fn window_heard(fake: &Fake, by_friend: bool) -> Option<WindowHeard> {
    if share::monitors().map_or(true, |monitors| monitors.is_empty()) {
        println!("skipped: no monitor is attached to the desktop");
        return None;
    }
    let screen = |mut config: (Config, Fake, Fake)| {
        config.0.video.source = VideoSource::Screen;
        config
    };
    let (host, code, to_host) = if by_friend {
        hosting(member("Mara", PC_SOUND_SINCE))
    } else {
        hosting(screen(sharer("Mara", PC_SOUND_SINCE, silence, fake)))
    };
    let mut ana = hand_watcher(&code, to_host);
    let bo = by_friend.then(|| {
        let config = screen(sharer("Bo", PC_SOUND_SINCE, silence, fake));
        joins(config, &code, to_host, 3)
    });
    let sharer = bo.as_ref().unwrap_or(&host);
    let window = Window::open(640, 360);
    sharer.room().share_window(60, window.id());
    let view = sharer
        .member
        .wait_for(WAIT, "the window share running", |v| {
            own_share(v).is_some() && v.share.running.is_some()
        });
    let share = own_share(&view).expect("granted");
    sharer.room().share_sound(true);
    ana.say(&watch(share));
    watched_by(&host, 1);
    let (sound, _) = listen(&mut ana, Duration::from_millis(400));
    assert!(sound.len() >= 25 && sound[0].first, "{:?}", flags(&sound));
    Some(WindowHeard {
        bo,
        host,
        ana,
        share,
        _window: window,
    })
}

// A shared window's capture says nothing of the lock screen, a UAC prompt or
// a lost session, so its share looks at the desktop itself, told here what
// it finds. Away, the PC sound ends with a LAST and the capture is let go;
// back, it comes again from a FIRST. After a rest with nobody watching, a
// new watcher hears nothing while the desktop is away, even while the look
// takes its time. Skipped where no monitor is attached, as on the runner.
#[test]
fn a_shared_window_sends_no_pc_sound_while_the_desktop_is_away() {
    let _alone = alone();
    let _looks = Looks::find(None);
    let fake = pc_sound(left_and_right, Duration::ZERO);
    let Some(heard) = window_heard(&fake, false) else {
        return;
    };
    let WindowHeard {
        host,
        mut ana,
        share,
        bo: _bo,
        _window,
    } = heard;
    let mut opens = 1;
    for away in [PauseReason::SecureDesktop, PauseReason::Disconnected] {
        Looks::set(Some(away.clone()));
        captures_open(&fake, opens, 0);
        let (gone, _) = listen(&mut ana, Duration::from_millis(300));
        let last = gone.iter().position(|packet| packet.last);
        assert_eq!(last, Some(gone.len() - 1), "{away}: {:?}", flags(&gone));
        Looks::set(None);
        opens += 1;
        captures_open(&fake, opens, 1);
        let (back, _) = listen(&mut ana, Duration::from_millis(400));
        assert!(
            back.len() >= 25 && back[0].first,
            "{away}: {:?}",
            flags(&back)
        );
    }
    ana.say(&stop_watching(share));
    watched_by(&host, 0);
    // Long enough for the facts to say nobody watches, so the share rests.
    listen(&mut ana, Duration::from_millis(1500));
    // While the look takes its time, only the rest holds the sound back.
    capture::fault::set_desktop(Some(PauseReason::SecureDesktop), Duration::from_millis(300));
    ana.say(&watch(share));
    watched_by(&host, 1);
    let (locked, _) = listen(&mut ana, Duration::from_millis(400));
    assert!(locked.is_empty(), "{:?}", flags(&locked));
    captures_open(&fake, opens, 0);
    Looks::set(None);
    opens += 1;
    captures_open(&fake, opens, 1);
    let (back, _) = listen(&mut ana, Duration::from_millis(400));
    assert!(back.len() >= 25 && back[0].first, "{:?}", flags(&back));
}

// Someone who stops watching and starts again within 250 ms gets the facts
// that count them up to 250 ms after their IDR ask, which the host sends at
// once: the share goes on from the ask instead of resting again on the
// facts that still say nobody watches, so the sound starts at once and
// plays on as one spell, whether the host shares or a friend does. The cap
// a viewer sends after its Watch changes the facts' rate on the way, but
// not who watches.
#[test]
fn watching_again_at_once_hears_the_sound_at_once() {
    let _alone = alone();
    let _looks = Looks::find(None);
    for (who, by_friend) in [("Mara", false), ("Bo", true)] {
        let fake = pc_sound(left_and_right, Duration::ZERO);
        let Some(heard) = window_heard(&fake, by_friend) else {
            return;
        };
        let WindowHeard {
            host,
            mut ana,
            share,
            bo: _bo,
            _window,
        } = heard;
        // Past the host's gaps: the facts that say Ana stopped go at once,
        // and so does the IDR ask of her next watch.
        listen(&mut ana, Duration::from_millis(600));
        let stopped = Instant::now();
        ana.say(&stop_watching(share));
        watched_by(&host, 0);
        // Long enough for the share to take the facts that say nobody
        // watches and rest, short of the 250 ms the host then holds new
        // facts back.
        listen(&mut ana, Duration::from_millis(120));
        // The facts that count Ana come 250 ms after the stop: the watch
        // goes 80 ms and more before them, so a share that waited for them
        // would send nothing in the 60 ms looked at.
        let after = stopped.elapsed();
        assert!(
            after < Duration::from_millis(170),
            "{who}: watching again {after:?} after stopping comes too late to tell"
        );
        ana.say(&watch_taking_hevc(share));
        ana.say(&watch_limits(share, 8_000));
        let (early, _) = listen(&mut ana, Duration::from_millis(60));
        assert!(
            early.first().is_some_and(|packet| packet.first),
            "{who}: {:?}",
            flags(&early)
        );
        let (then, _) = listen(&mut ana, Duration::from_millis(400));
        assert!(
            then.len() >= 35 && then.iter().all(|packet| !packet.first && !packet.last),
            "{who}: {:?}",
            flags(&then)
        );
    }
}

// Share after share with a tone playing: Share sound is off at the start of
// each, and no packet ever names a share whose switch was off when it was
// captured.
#[test]
fn no_packet_names_a_share_whose_switch_is_off() {
    const SLACK_US: u64 = 5_000;
    let _alone = alone();
    let fake = pc_sound(left_and_right, Duration::ZERO);
    let (host, code, to_host) = hosting(sharer("Mara", PC_SOUND_SINCE, silence, &fake));
    let mut ana = hand_watcher(&code, to_host);
    let mut turned_on: Vec<(u32, u64, u64)> = Vec::new();
    let mut heard = Vec::new();
    let mut last = None;
    for _ in 0..4 {
        host.room().share(60, None);
        let view = host.member.wait_for(WAIT, "a new share", |v| {
            own_share(v).is_some_and(|number| Some(number) != last)
        });
        let share = own_share(&view).expect("granted");
        last = Some(share);
        assert!(
            !view.share.own_sound.on,
            "share {share} began with sound on"
        );
        ana.say(&watch(share));
        watched_by(&host, 1);
        let (quiet, _) = listen(&mut ana, Duration::from_millis(200));
        heard.extend(quiet);
        let on_us = wall_clock_us();
        host.room().share_sound(true);
        let (sound, _) = listen(&mut ana, Duration::from_millis(300));
        assert!(
            sound.iter().any(|packet| packet.share == share),
            "share {share}: nothing came"
        );
        heard.extend(sound);
        host.room().stop_sharing();
        let off_us = wall_clock_us();
        turned_on.push((share, on_us, off_us));
        host.member
            .wait_for(WAIT, "the share over", |v| v.share.current.is_none());
    }
    let (tail, _) = listen(&mut ana, Duration::from_millis(200));
    heard.extend(tail);
    for packet in &heard {
        let Some(&(share, on_us, off_us)) =
            turned_on.iter().find(|(share, ..)| *share == packet.share)
        else {
            panic!("a packet names share {}, never turned on", packet.share);
        };
        assert!(
            packet.captured + SLACK_US >= on_us && packet.captured <= off_us + SLACK_US,
            "share {share}: captured at {}, on from {on_us} to {off_us}",
            packet.captured
        );
    }
}

// The sharer's Mute and Deafen are its microphone's and its speakers', not
// its share's: the PC sound goes on as it was, in the same spell.
#[test]
fn the_sharers_mute_and_deafen_change_nothing() {
    let _alone = alone();
    let fake = pc_sound(left_and_right, Duration::ZERO);
    let (host, mut ana, _, before) = hand_watches(&fake);
    host.room().mute(true);
    host.room().deafen(true);
    let (after, told) = listen(&mut ana, Duration::from_millis(400));
    println!("{} frames before, {} after", before.len(), after.len());
    assert!(
        after.len() * 10 >= before.len() * 8,
        "{} of {}",
        after.len(),
        before.len()
    );
    assert!(
        after.iter().all(|packet| !packet.first && !packet.last),
        "{:?}",
        flags(&after)
    );
    assert!(told.is_empty(), "{told:?}");
    assert!(host.view().share.own_sound.on);
}

static QUIET_NOW: AtomicBool = AtomicBool::new(false);

fn tone_until_quiet(frame: u64, channel: u16) -> f32 {
    if QUIET_NOW.load(Ordering::Relaxed) {
        0.0
    } else {
        left_and_right(frame, channel)
    }
}

// Silence ends a spell after a drawn 200 to 800 ms: 20 to 80 silent frames
// go out, the last with LAST, and nothing more while it lasts. The next
// sound starts a spell from a FIRST.
#[test]
fn silence_ends_a_spell_and_the_next_sound_starts_one() {
    let _alone = alone();
    QUIET_NOW.store(false, Ordering::Relaxed);
    let fake = pc_sound(tone_until_quiet, Duration::ZERO);
    let (_host, mut ana, _, _) = hand_watches(&fake);
    QUIET_NOW.store(true, Ordering::Relaxed);
    let (quiet, _) = listen(&mut ana, Duration::from_millis(1200));
    QUIET_NOW.store(false, Ordering::Relaxed);
    let (back, _) = listen(&mut ana, Duration::from_millis(300));
    let last = quiet.iter().position(|packet| packet.last);
    assert_eq!(last, Some(quiet.len() - 1), "{:?}", flags(&quiet));
    // The frame the silence began in, and one more of the tone at most.
    assert!((20..=82).contains(&quiet.len()), "{}", quiet.len());
    assert!(back.len() >= 15 && back[0].first, "{:?}", flags(&back));
}

// PC sound that cannot open turns Share sound off with a line that says
// why, and the share goes on; its watcher never hears that the sound is on.
#[test]
fn a_capture_that_cannot_start_turns_share_sound_off() {
    let _alone = alone();
    let fake = pc_sound(left_and_right, Duration::ZERO);
    fake.fail_opens(Some(AudioError::PcSound {
        code: 0x8889_0008,
        text: String::from("Windows would not give its sound as 48 kHz stereo"),
        lost: false,
    }));
    let (host, code, to_host) = hosting(sharer("Mara", PC_SOUND_SINCE, silence, &fake));
    let ana = joins(member("Ana", PC_SOUND_SINCE), &code, to_host, 2);
    let share = shares(&host);
    watches(&ana, share);
    watched_by(&host, 1);
    host.room().share_sound(true);
    let view = host.member.wait_for(WAIT, "Share sound off and why", |v| {
        !v.share.own_sound.on && v.share.problem.is_some()
    });
    let line = "PC sound could not start: Windows would not give its sound as 48 kHz stereo (0x88890008). The share goes on without it.";
    assert_eq!(view.share.problem.as_deref(), Some(line));
    assert!(
        view.chat
            .iter()
            .any(|said| said.kind == LineKind::Problem && said.text == line)
    );
    assert_eq!(own_share(&view), Some(share));
    ana.member.holds_for(
        Duration::from_millis(300),
        "Ana told the sound is on",
        |v| !v.share.watched_sound,
    );
    assert_eq!(ana.view().numbers.dropped_bad, 0);
}

// Lost while it runs, PC sound opens again by itself a second later: its
// watcher hears it is off and then on again once it has opened, and Share
// sound stays on meanwhile. Lost again and refused when it opens again, it
// has stopped: Share sound goes off with a line that says why.
#[test]
fn lost_pc_sound_comes_back_by_itself_or_turns_share_sound_off() {
    let _alone = alone();
    let fake = pc_sound(left_and_right, Duration::ZERO);
    let (host, mut ana, share, _) = hand_watches(&fake);
    fake.unplug("pc");
    let (_, told) = listen(&mut ana, Duration::from_millis(300));
    assert_eq!(told, [(share, false)]);
    let own = host.view().share.own_sound;
    assert!(own.on);
    let lost = own.lost.expect("why the sound stopped, under the switch");
    assert!(lost.contains("stopped"), "{lost}");
    fake.plug("pc", "Test PC sound");
    let (back, told) = listen(&mut ana, Duration::from_millis(1500));
    assert_eq!(told, [(share, true)]);
    assert!(back.len() >= 25 && back[0].first, "{:?}", flags(&back));
    assert_eq!(host.view().share.own_sound.lost, None);

    fake.fail_opens(Some(AudioError::PcSound {
        code: 0x8889_0008,
        text: String::from("Windows would not give its sound as 48 kHz stereo"),
        lost: false,
    }));
    fake.unplug("pc");
    let view = host.member.wait_for(WAIT, "Share sound off and why", |v| {
        !v.share.own_sound.on && v.share.problem.is_some()
    });
    assert_eq!(
        view.share.problem.as_deref(),
        Some(
            "PC sound stopped: Windows would not give its sound as 48 kHz stereo (0x88890008). Turn Share sound on to try again."
        )
    );
    let (_, told) = listen(&mut ana, Duration::from_millis(200));
    assert_eq!(told, [(share, false)]);
}

fn captures_open(fake: &Fake, opens: usize, open: usize) {
    poll(WAIT, "the captures as asked", || {
        let record = fake.record();
        (record.opens.len() == opens && record.at_once == open).then_some(())
    });
}

// Stop sharing and Leave let the capture go at once, and a new share
// starts with Share sound off.
#[test]
fn stop_and_leave_let_the_capture_go() {
    let _alone = alone();
    let fake = pc_sound(left_and_right, Duration::ZERO);
    let (mut host, _, _) = hosting(sharer("Mara", PC_SOUND_SINCE, silence, &fake));
    shares(&host);
    host.room().share_sound(true);
    captures_open(&fake, 1, 1);
    host.room().stop_sharing();
    captures_open(&fake, 1, 0);
    shares(&host);
    assert!(!host.view().share.own_sound.on);
    host.room().share_sound(true);
    captures_open(&fake, 2, 1);
    host.member.leave();
    captures_open(&fake, 2, 0);
}

// A share that cannot start ends a moment after it is granted. With Share
// sound turned on in that moment, the capture goes with the share.
#[test]
fn a_failed_share_lets_the_capture_go() {
    let _alone = alone();
    let fake = pc_sound(left_and_right, Duration::ZERO);
    let mut config = sharer("Mara", PC_SOUND_SINCE, silence, &fake);
    config.0.video = VideoConfig {
        source: VideoSource::Pattern {
            width: 0,
            height: 0,
            busy: false,
        },
        show: Show::NoActivate,
        ..VideoConfig::default()
    };
    let host = Voiced::host_with(config);
    let problems = |v: &View| {
        v.chat
            .iter()
            .filter(|said| said.kind == LineKind::Problem)
            .count()
    };
    for attempt in 0.. {
        assert!(
            attempt < 20,
            "every share failed before Share sound went on"
        );
        let (before, opens) = (problems(&host.view()), fake.record().opens.len());
        host.room().share(60, None);
        host.room().share_sound(true);
        host.member.wait_for(WAIT, "the share failed", |v| {
            problems(v) > before && v.share.own == OwnShare::Off
        });
        // A capture started just before the end opens and closes on its own
        // thread.
        thread::sleep(Duration::from_millis(200));
        let record = fake.record();
        assert_eq!(record.at_once, 0, "a capture outlived its share");
        if record.opens.len() > opens {
            break;
        }
    }
    assert!(!host.view().share.own_sound.on);
}
