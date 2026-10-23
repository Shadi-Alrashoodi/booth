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

// Bo's word on whether his sound goes out, as the host passes it on. Say it
// at the rig's start: the host here never reads Ana's acks, so asked once its
// earlier frames are 200 ms old it would send one of them again instead.
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

// While the speakers take nothing from their orders, as while Windows has no
// output to play on, the oldest ear they never took stops new ones being
// made: each would wait there with its decoder until the speakers came
// back. Once they take it, a frame makes one again.
#[test]
fn speakers_away_make_no_ears() {
    let start = Instant::now();
    let (mut rig, mut session, mut speaker) = watching(peer::PC_SOUND_SINCE, start);
    let frames = stereo_frames(3);
    let from_bo = |rig: &mut Rig, session: &mut Session, seq: u16, at: Instant| {
        let frame = &frames[usize::from(seq)];
        let packet = relayed(session, BO_SLOT, SHARE, seq, true, frame);
        rig.deliver(&packet, at);
    };
    bo_says(&mut rig, &mut session, true, start);
    from_bo(&mut rig, &mut session, 0, start);
    assert!(rig.client.view(start, 0).numbers.sound.is_some());
    bo_says(&mut rig, &mut session, false, start);
    let later = start + Duration::from_secs(1);
    bo_says(&mut rig, &mut session, true, start);
    from_bo(&mut rig, &mut session, 1, later);
    let view = rig.client.view(later, 0);
    assert!(
        view.numbers.sound.is_none(),
        "an ear the speakers cannot take"
    );
    assert_eq!(view.numbers.sound_dropped, 1);
    loudest(&mut speaker);
    from_bo(&mut rig, &mut session, 2, later);
    assert!(rig.client.view(later, 0).numbers.sound.is_some());
}

// Ears made every 500 ms, faster than TAKEN_WITHIN, while the speakers take
// nothing, here by watching again each time: the oldest ear they have not
// taken is the one timed, so once it is a second old no more are made.
#[test]
fn ears_made_fast_stop_while_the_speakers_are_away() {
    let start = Instant::now();
    let (mut rig, mut session, _speaker) = watching(peer::PC_SOUND_SINCE, start);
    let frames = stereo_frames(7);
    for step in 0..7u16 {
        let at = start + Duration::from_millis(500) * u32::from(step);
        rig.client.watch(SHARE, false, at, &rig.socket);
        rig.client.watch(SHARE, true, at, &rig.socket);
        let frame = &frames[usize::from(step)];
        let packet = relayed(&mut session, BO_SLOT, SHARE, step, true, frame);
        rig.deliver(&packet, at);
    }
    // Ears at 0 and 0.5 s; from 1 s on the first is a second old and waits.
    let view = rig.client.view(start + Duration::from_secs(3), 0);
    assert_eq!(view.numbers.sound_dropped, 5);
}

// A PC sound volume set before any of Bo's sound came holds from his first
// frame: at 0 not a sample of it plays, and turned back up it does.
#[test]
fn a_pc_sound_volume_holds_from_the_first_frame() {
    let start = Instant::now();
    let (mut rig, mut session, mut speaker) = watching(peer::PC_SOUND_SINCE, start);
    rig.client.set_sound_volume(BO, 0);
    let frames = stereo_frames(24);
    for seq in 0..24u16 {
        if seq == 12 {
            rig.client.set_sound_volume(BO, 100);
        }
        let frame = &frames[usize::from(seq)];
        let packet = relayed(&mut session, BO_SLOT, SHARE, seq, seq == 0, frame);
        rig.deliver(&packet, start);
        let heard = loudest(&mut speaker);
        if seq < 12 {
            assert_eq!(heard, 0.0, "frame {seq}");
        } else if seq >= 16 {
            assert!(heard > 0.1, "frame {seq}: {heard}");
        }
    }
    assert!(rig.client.view(start, 0).numbers.sound.is_some());
}

// How far the sound is from the picture needs no clock offsets, so it shows
// before they are known, while capture to ear cannot: Bo's sound played
// here, beside pictures of his share the viewer showed, each captured 3 ms
// into a sound frame.
#[test]
fn sound_after_the_picture_needs_no_clock_offset() {
    let start = Instant::now();
    let (mut rig, mut session, mut speaker) = watching(peer::PC_SOUND_SINCE, start);
    let frames = stereo_frames(140);
    let stamp = |seq: u16| 1_000_000 + u64::from(seq) * 10_000 + 3_000;
    let mut play = |rig: &mut Rig, session: &mut Session, seq: u16| {
        let frame = &frames[usize::from(seq)];
        let packet = relayed(session, BO_SLOT, SHARE, seq, seq == 0, frame);
        rig.deliver(&packet, start);
        loudest(&mut speaker);
    };
    let mut shown = Vec::new();
    for seq in 0..130 {
        play(&mut rig, &mut session, seq);
        shown.push((stamp(seq), Instant::now()));
    }
    // Five pictures whose sound has not come yet wait for it.
    for seq in 130..135 {
        shown.push((stamp(seq), Instant::now()));
    }
    rig.client.screen.watching.shown(SHARE, &shown);
    let now = Instant::now();
    rig.client.screen_work(now, &rig.socket);
    let sound = rig.client.view(now, 0).numbers.sound.expect("sound heard");
    assert!(sound.capture_to_ear.is_none(), "no clock offsets yet");
    let after = sound.after_picture.expect("how far apart");
    assert!(after.pictures >= 120, "{after:?}");
    // 3 ms into the frame and the codec's 2.5 ms lookahead, less the moment
    // between the frame leaving and the picture's time being taken.
    assert!((4.0..6.0).contains(&after.median_ms), "{after:?}");

    // The timer thread wakes with no pictures too: the five keep waiting
    // for the next handover, by when their sound has played.
    rig.client.screen_work(now, &rig.socket);
    for seq in 130..140 {
        play(&mut rig, &mut session, seq);
    }
    rig.client
        .screen
        .watching
        .shown(SHARE, &[(stamp(136), Instant::now())]);
    let now = Instant::now();
    rig.client.screen_work(now, &rig.socket);
    let sound = rig.client.view(now, 0).numbers.sound.expect("sound heard");
    assert_eq!(sound.after_picture.map(|after| after.pictures), Some(136));

    // Off lets the ear and its numbers go: the next sound starts afresh,
    // before the timer thread's next round too.
    bo_says(&mut rig, &mut session, false, start);
    let again = stereo_frames(1);
    let packet = relayed(&mut session, BO_SLOT, SHARE, 200, true, &again[0]);
    rig.deliver(&packet, now);
    loudest(&mut speaker);
    let sound = rig.client.view(now, 0).numbers.sound.expect("sound again");
    assert_eq!(sound.after_picture, None);
    rig.client.screen_work(now, &rig.socket);
    let sound = rig.client.view(now, 0).numbers.sound.expect("sound again");
    assert_eq!(sound.after_picture, None);
}

// With no more pictures, as from a still screen, how far the sound is from
// the picture still ages out of the numbers, and the log hears how the
// sound goes every 10 s from the first round that had an ear.
#[test]
fn sound_after_the_picture_ages_and_is_logged_with_no_pictures() {
    let start = Instant::now();
    let (mut rig, mut session, _) = watching(peer::PC_SOUND_SINCE, start);
    let (orders_in, orders) = crossbeam_channel::unbounded();
    let clock = rig.client.clock();
    let (log, lines) = Log::capture(16);
    rig.client.sound = Sound::new(orders_in, clock, log, start);
    let mut speaker = Speaker::new(orders, Arc::clone(rig.client.talk.shared()), clock);
    let mut shown = Vec::new();
    for (seq, frame) in stereo_frames(120).iter().enumerate() {
        let seq = seq as u16;
        let packet = relayed(&mut session, BO_SLOT, SHARE, seq, seq == 0, frame);
        rig.deliver(&packet, start);
        loudest(&mut speaker);
        shown.push((1_000_000 + u64::from(seq) * 10_000 + 3_000, Instant::now()));
    }
    rig.client.screen.watching.shown(SHARE, &shown);
    let now = Instant::now();
    let at = |secs: u64| now + Duration::from_secs(secs);
    let after = |rig: &Rig, secs: u64| {
        let numbers = rig.client.view(at(secs), 0).numbers;
        numbers.sound.expect("sound heard").after_picture
    };
    rig.client.screen_work(now, &rig.socket);
    assert!(after(&rig, 0).is_some());
    rig.client.screen_work(at(9), &rig.socket);
    assert!(after(&rig, 9).is_some(), "still inside the window");
    assert!(lines.lines().is_empty(), "no line before 10 s");
    rig.client.screen_work(at(10), &rig.socket);
    assert_eq!(after(&rig, 10), None, "out of the window");
    let said = lines.lines();
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(
        said[0].starts_with(&format!("watch {SHARE}: pc sound: depth ")),
        "{said:?}"
    );
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
