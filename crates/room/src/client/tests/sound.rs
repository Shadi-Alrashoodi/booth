// PC sound on a watcher: what the host passes on reaches the speakers only
// from the share this PC watches, and the sharer's word on it decides when
// the ear goes.

use super::*;
use crate::control::EntryShare;
use crate::sound::SoundFrame;
use crate::talk::Speaker;
use voice::stereo::{Encoder, FRAME, MAX_PACKET};

const BO: [u8; 32] = [9; 32];
const BO_SLOT: u8 = 2;
const SHARE: u32 = 4;

// Ana's client, standing for `version`, in a room where Bo shares from his
// slot and Ana watches, with her speakers driven by hand.
fn watching(version: Version, start: Instant) -> (Rig, Session, Speaker) {
    let (mut rig, mut session) = Rig::connected(Timers::default(), start);
    rig.client.version = version;
    let (orders_in, orders) = crossbeam_channel::unbounded();
    let clock = rig.client.clock();
    rig.client.sound = Sound::new(orders_in, clock, Log::off(), start);
    let speaker = Speaker::new(orders, Arc::clone(rig.client.talk.shared()), clock);
    let entry = |key: [u8; 32], slot: u8, name: &str, share| control::Entry {
        key,
        slot,
        name: String::from(name),
        rtt_ms: None,
        is_host: slot == 0,
        joined_by_invite: slot != 0,
        reconnecting: false,
        share,
        controlling: false,
    };
    let roster = Message::Roster(Roster {
        room: String::from("Tuesday night"),
        entries: vec![
            entry(*rig.host.identity.public(), 0, "Mara", None),
            entry(*rig.client.identity.public(), 1, "Ana", None),
            entry(
                BO,
                BO_SLOT,
                "Bo",
                Some(EntryShare {
                    number: SHARE,
                    fps: 60,
                }),
            ),
        ],
    });
    let packet = rig.host.say(&mut session, &roster, start);
    rig.deliver(&packet, start);
    assert!(rig.client.watch(SHARE, true, start, &rig.socket));
    (rig, session, speaker)
}

// 10 ms stereo frames of a tone at 0.4 on both sides, as a sharer's encoder
// makes them.
fn stereo_frames(count: usize) -> Vec<Vec<u8>> {
    let mut encoder = Encoder::new().unwrap();
    (0..count)
        .map(|i| {
            let pcm: Vec<f32> = (0..FRAME * 2)
                .map(|n| 0.4 * ((i * FRAME + n / 2) as f32 * 0.06).sin())
                .collect();
            let mut out = [0u8; MAX_PACKET];
            let len = encoder.encode(&pcm, &mut out).unwrap();
            out[..len].to_vec()
        })
        .collect()
}

// PC sound for `share` as the host passes it on from `slot`.
fn relayed(
    session: &mut Session,
    slot: u8,
    share: u32,
    seq: u16,
    first: bool,
    frame: &[u8],
) -> Vec<u8> {
    let mut payload = Vec::new();
    SoundFrame {
        slot,
        share,
        seq,
        captured: 1_000_000 + u64::from(seq) * 10_000,
        first,
        last: false,
        frame,
    }
    .write_relayed(slot, &mut payload);
    seal(session, Channel::Sound, &payload)
}

// Bo's word on whether his sound goes out, as the host passes it on.
fn bo_says(rig: &mut Rig, session: &mut Session, on: bool, now: Instant) {
    let word = Message::ShareSound { share: SHARE, on };
    let packet = rig.host.say(session, &word, now);
    rig.deliver(&packet, now);
}

// The loudest sample of the speakers' next 10 ms.
fn loudest(speaker: &mut Speaker) -> f32 {
    let mut out = [0.0f32; 960];
    speaker.fill(&mut out);
    out.iter().fold(0.0, |most, s| most.max(s.abs()))
}

// Only Bo's slot and his share's number play. Sound from another slot or for
// another share goes quietly, as video does; a packet that does not parse is
// bad. The end of the watch lets the ear go.
#[test]
fn only_the_share_watched_reaches_the_speakers() {
    let start = Instant::now();
    let (mut rig, mut session, mut speaker) = watching(peer::PC_SOUND_SINCE, start);
    let frames = stereo_frames(30);
    let bad = rig.client.drops.bad;
    for seq in 0..5u16 {
        let frame = &frames[usize::from(seq)];
        rig.deliver(
            &relayed(&mut session, 3, SHARE, seq, seq == 0, frame),
            start,
        );
        let other_share = relayed(&mut session, BO_SLOT, SHARE + 1, seq, seq == 0, frame);
        rig.deliver(&other_share, start);
        assert_eq!(loudest(&mut speaker), 0.0);
    }
    assert_eq!(rig.client.drops.bad, bad);
    assert!(rig.client.view(start, 0).numbers.sound.is_none());

    let mut heard = 0.0f32;
    for seq in 5..25u16 {
        let frame = &frames[usize::from(seq)];
        let packet = relayed(&mut session, BO_SLOT, SHARE, seq, seq == 5, frame);
        rig.deliver(&packet, start);
        heard = heard.max(loudest(&mut speaker));
    }
    let view = rig.client.view(start, 0);
    let numbers = view.numbers.sound.expect("numbers for the pc sound heard");
    println!("bo's pc sound peaked at {heard:.3} on ana's speakers: {numbers:?}");
    assert!((0.15..0.25).contains(&heard), "{heard}");
    assert_eq!(numbers.depth_ms, 10);
    assert_eq!(numbers.made_up, 0);
    assert_eq!(view.numbers.sound_dropped, 0);
    assert_eq!(rig.client.drops.bad, bad);

    let mut payload = Vec::new();
    SoundFrame {
        slot: BO_SLOT,
        share: SHARE,
        seq: 25,
        captured: 1,
        first: false,
        last: false,
        frame: &frames[25],
    }
    .write_relayed(BO_SLOT, &mut payload);
    // A flag Booth does not set, at offset 16.
    payload[16] |= 0x04;
    rig.deliver(&seal(&mut session, Channel::Sound, &payload), start);
    assert_eq!(rig.client.drops.bad, bad + 1);

    assert!(rig.client.watch(SHARE, false, start, &rig.socket));
    assert!(rig.client.view(start, 0).numbers.sound.is_none());
    // Its last 2.5 ms fade out, and then nothing more plays.
    loudest(&mut speaker);
    for seq in 26..30u16 {
        let frame = &frames[usize::from(seq)];
        rig.deliver(
            &relayed(&mut session, BO_SLOT, SHARE, seq, false, frame),
            start,
        );
        assert_eq!(loudest(&mut speaker), 0.0);
    }
    assert_eq!(rig.client.drops.bad, bad + 1);
}

// Bo's word that his sound is off lets the ear go, fading. Frames captured
// before his press still come after the word and make no new ear, counted
// with the sound dropped and not as bad; a frame that starts a spell from a
// reset encoder does. His word that it is on again takes any frame, since
// that first one can be lost.
#[test]
fn off_lets_the_ear_go_and_a_first_frame_brings_it_back() {
    let start = Instant::now();
    let (mut rig, mut session, mut speaker) = watching(peer::PC_SOUND_SINCE, start);
    let frames = stereo_frames(45);
    let from_bo = |rig: &mut Rig, session: &mut Session, seq: u16, first: bool| {
        let frame = &frames[usize::from(seq)];
        let packet = relayed(session, BO_SLOT, SHARE, seq, first, frame);
        rig.deliver(&packet, start);
    };
    bo_says(&mut rig, &mut session, true, start);
    assert!(rig.client.view(start, 0).share.watched_sound);
    for seq in 0..10 {
        from_bo(&mut rig, &mut session, seq, seq == 0);
        loudest(&mut speaker);
    }
    from_bo(&mut rig, &mut session, 10, false);
    bo_says(&mut rig, &mut session, false, start);
    let view = rig.client.view(start, 0);
    assert!(!view.share.watched_sound);
    assert!(view.numbers.sound.is_none());
    let mut out = [0.0f32; 960];
    speaker.fill(&mut out);
    let (faded, _) = out.as_chunks::<2>();
    assert!(faded[..60].iter().any(|[left, _]| left.abs() > 0.05));
    assert!(
        faded[120..]
            .iter()
            .all(|&[left, right]| left == 0.0 && right == 0.0)
    );

    let bad = rig.client.drops.bad;
    for seq in 11..16 {
        from_bo(&mut rig, &mut session, seq, false);
        assert_eq!(loudest(&mut speaker), 0.0);
    }
    let view = rig.client.view(start, 0);
    assert!(view.numbers.sound.is_none());
    assert_eq!(view.numbers.sound_dropped, 5);
    assert_eq!(rig.client.drops.bad, bad);

    let mut heard = 0.0f32;
    for seq in 16..30 {
        from_bo(&mut rig, &mut session, seq, seq == 16);
        heard = heard.max(loudest(&mut speaker));
    }
    assert!(heard > 0.15, "{heard}");
    assert!(rig.client.view(start, 0).numbers.sound.is_some());

    bo_says(&mut rig, &mut session, false, start);
    bo_says(&mut rig, &mut session, true, start);
    let mut heard = 0.0f32;
    for seq in 30..45 {
        from_bo(&mut rig, &mut session, seq, false);
        heard = heard.max(loudest(&mut speaker));
    }
    assert!(heard > 0.15, "{heard}");
    let view = rig.client.view(start, 0);
    assert!(view.share.watched_sound);
    assert!(view.numbers.sound.is_some());
    assert_eq!(view.numbers.sound_dropped, 5);
    assert_eq!(rig.client.drops.bad, bad);
}

// A client standing for a copy older than PC_SOUND_SINCE counts channel 8
// bad, as such a copy does, and plays none of it.
#[test]
fn a_client_standing_for_1_2_0_counts_pc_sound_bad() {
    let start = Instant::now();
    let older = Version {
        major: 1,
        minor: 2,
        patch: 0,
    };
    let (mut rig, mut session, mut speaker) = watching(older, start);
    let frames = stereo_frames(5);
    let bad = rig.client.drops.bad;
    for seq in 0..5u16 {
        let frame = &frames[usize::from(seq)];
        rig.deliver(
            &relayed(&mut session, BO_SLOT, SHARE, seq, seq == 0, frame),
            start,
        );
        assert_eq!(loudest(&mut speaker), 0.0);
    }
    assert_eq!(rig.client.drops.bad, bad + 5);
    assert!(rig.client.view(start, 0).numbers.sound.is_none());
}
