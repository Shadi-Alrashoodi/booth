// PC sound on a watcher: what the host passes on reaches the speakers only
// from the share this PC watches, and the sharer's word on it decides when
// the ear goes. Neither of the sharer's words counts before the host's
// answer to this PC's last Watch.

use super::*;
use crate::control::EntryShare;
use crate::sound::SoundFrame;
use crate::talk::Speaker;
use crate::view::{ShareState, WatchCap};
use voice::stereo::{Encoder, FRAME, MAX_PACKET};

const BO: [u8; 32] = [9; 32];
const BO_SLOT: u8 = 2;
const SHARE: u32 = 4;

// Ana's client, standing for `version`, in a room where Bo shares from his
// slot and Ana has pressed Watch, with her speakers driven by hand.
fn watch_pressed(version: Version, start: Instant) -> (Rig, Session, Speaker) {
    let (mut rig, mut session) = Rig::connected(Timers::default(), start);
    rig.client.version = version;
    let (orders_in, orders) = crossbeam_channel::unbounded();
    let clock = rig.client.clock();
    rig.client.sound = Sound::new(orders_in, clock, Log::off(), start);
    let speaker = Speaker::new(orders, Arc::clone(rig.client.talk.shared()), clock);
    bo_shares(&mut rig, &mut session, Some(SHARE), start);
    assert!(rig.client.watch(SHARE, true, start, &rig.socket));
    (rig, session, speaker)
}

// The host's roster: Mara hosts, and Bo shares `share` from his slot, or
// nothing.
fn bo_shares(rig: &mut Rig, session: &mut Session, share: Option<u32>, now: Instant) {
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
                share.map(|number| EntryShare { number, fps: 60 }),
            ),
        ],
    });
    let packet = rig.host.say(session, &roster, now);
    rig.deliver(&packet, now);
}

// And answered at her Watch as a host on 1.3.0 answers a client on it: Bo's
// share is live and his sound off, as he has not said otherwise. An older
// client is told nothing.
fn watching(version: Version, start: Instant) -> (Rig, Session, Speaker) {
    let (mut rig, mut session, speaker) = watch_pressed(version, start);
    if version >= peer::PC_SOUND_SINCE {
        answer(&mut rig, &mut session, ShareState::Live, false, start);
    }
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

// Bo's word on how his share stands, as the host passes it on, at the same
// times.
fn bo_stands(rig: &mut Rig, session: &mut Session, state: ShareState, now: Instant) {
    let word = Message::ShareState {
        share: SHARE,
        state,
    };
    let packet = rig.host.say(session, &word, now);
    rig.deliver(&packet, now);
}

// The host's answer to a Watch of Bo's share: how it stands and whether his
// sound goes out, at the same times.
fn answer(rig: &mut Rig, session: &mut Session, state: ShareState, on: bool, now: Instant) {
    let told = Message::WatchAnswer {
        share: SHARE,
        state,
        sound: on,
    };
    let packet = rig.host.say(session, &told, now);
    rig.deliver(&packet, now);
}

// How Ana's panel shows Bo's share standing, and his sound word.
fn words(rig: &Rig) -> (ShareState, Option<bool>) {
    let share = rig.client.share_view();
    (share.watched_state, share.watched_sound)
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
    assert_eq!(rig.client.view(start, 0).share.watched_sound, Some(true));
    for seq in 0..10 {
        from_bo(&mut rig, &mut session, seq, seq == 0);
        loudest(&mut speaker);
    }
    from_bo(&mut rig, &mut session, 10, false);
    bo_says(&mut rig, &mut session, false, start);
    let view = rig.client.view(start, 0);
    assert_eq!(view.share.watched_sound, Some(false));
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
    assert_eq!(view.share.watched_sound, Some(true));
    assert!(view.numbers.sound.is_some());
    assert_eq!(view.numbers.sound_dropped, 5);
    assert_eq!(rig.client.drops.bad, bad);
}

// Told at her Watch that Bo's sound is off, as while his on waits at the
// host, Ana hears the frames that come meanwhile though none starts a spell
// for her: the first word of a watch only sets the word.
#[test]
fn an_off_at_the_watch_holds_no_frame_back() {
    let start = Instant::now();
    let (mut rig, mut session, mut speaker) = watching(peer::PC_SOUND_SINCE, start);
    let frames = stereo_frames(15);
    let bad = rig.client.drops.bad;
    assert_eq!(rig.client.view(start, 0).share.watched_sound, Some(false));
    let mut heard = 0.0f32;
    for seq in 5..15u16 {
        let frame = &frames[usize::from(seq)];
        let packet = relayed(&mut session, BO_SLOT, SHARE, seq, false, frame);
        rig.deliver(&packet, start);
        heard = heard.max(loudest(&mut speaker));
    }
    assert!(heard > 0.0, "{heard}");
    let view = rig.client.view(start, 0);
    assert_eq!(view.share.watched_sound, Some(false));
    assert!(view.numbers.sound.is_some());
    assert_eq!(view.numbers.sound_dropped, 0);
    assert_eq!(rig.client.drops.bad, bad);
}

// The word Ana is told at her Watch can come after the first frames, its
// datagram lost or overtaken. They make an ear, and that word, off while
// Bo's on waits at the host, does not let it go: the first word of a watch
// only sets the word.
#[test]
fn an_off_at_the_watch_lets_no_ear_go() {
    let start = Instant::now();
    let (mut rig, mut session, mut speaker) = watch_pressed(peer::PC_SOUND_SINCE, start);
    let frames = stereo_frames(15);
    let bad = rig.client.drops.bad;
    let mut from_bo = |rig: &mut Rig, session: &mut Session, seq: u16| {
        let frame = &frames[usize::from(seq)];
        let packet = relayed(session, BO_SLOT, SHARE, seq, false, frame);
        rig.deliver(&packet, start);
        loudest(&mut speaker)
    };
    let mut heard = 0.0f32;
    for seq in 5..10 {
        heard = heard.max(from_bo(&mut rig, &mut session, seq));
    }
    assert!(heard > 0.1, "{heard}");
    assert_eq!(rig.client.view(start, 0).share.watched_sound, None);
    answer(&mut rig, &mut session, ShareState::Live, false, start);
    for seq in 10..15 {
        let heard = from_bo(&mut rig, &mut session, seq);
        assert!(heard > 0.1, "frame {seq}: {heard}");
    }
    let view = rig.client.view(start, 0);
    assert_eq!(view.share.watched_sound, Some(false));
    assert!(view.numbers.sound.is_some());
    assert_eq!(view.numbers.sound_dropped, 0);
    assert_eq!(rig.client.drops.bad, bad);
}

// Bo turns his sound on and off before any frame of it reaches Ana, so his
// off finds no ear to let go. The frames captured before his press that
// come after it still make none, each counted with the sound dropped and
// not as bad, until a frame that starts a spell from a reset encoder does.
#[test]
fn an_off_that_finds_no_ear_still_holds_frames_back() {
    let start = Instant::now();
    let (mut rig, mut session, mut speaker) = watching(peer::PC_SOUND_SINCE, start);
    let frames = stereo_frames(30);
    let bad = rig.client.drops.bad;
    let from_bo = |rig: &mut Rig, session: &mut Session, seq: u16, first: bool| {
        let frame = &frames[usize::from(seq)];
        let packet = relayed(session, BO_SLOT, SHARE, seq, first, frame);
        rig.deliver(&packet, start);
    };
    bo_says(&mut rig, &mut session, true, start);
    bo_says(&mut rig, &mut session, false, start);
    assert_eq!(rig.client.view(start, 0).share.watched_sound, Some(false));
    for seq in 5..15 {
        from_bo(&mut rig, &mut session, seq, false);
        assert_eq!(loudest(&mut speaker), 0.0, "frame {seq}");
    }
    let view = rig.client.view(start, 0);
    assert!(view.numbers.sound.is_none());
    assert_eq!(view.numbers.sound_dropped, 10);

    from_bo(&mut rig, &mut session, 15, true);
    assert!(rig.client.view(start, 0).numbers.sound.is_some());
    let mut heard = loudest(&mut speaker);
    for seq in 16..30 {
        from_bo(&mut rig, &mut session, seq, false);
        heard = heard.max(loudest(&mut speaker));
    }
    assert!(heard > 0.15, "{heard}");
    let view = rig.client.view(start, 0);
    assert!(view.numbers.sound.is_some());
    assert_eq!(view.numbers.sound_dropped, 10);
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
// frame: at 0 not a sample of it plays and the numbers say it is muted, and
// turned back up it plays.
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
        if seq == 11 {
            let sound = rig.client.view(start, 0).numbers.sound;
            assert_eq!(sound.map(|sound| sound.muted), Some(true), "at 0");
        }
    }
    let sound = rig.client.view(start, 0).numbers.sound;
    assert_eq!(sound.map(|sound| sound.muted), Some(false));
    for percent in [1, 150] {
        rig.client.set_sound_volume(BO, percent);
        let sound = rig.client.view(start, 0).numbers.sound;
        assert_eq!(sound.map(|sound| sound.muted), Some(false), "{percent}");
    }
    rig.client.set_sound_volume(BO, 100);
    rig.client.set_sound_volume([7; 32], 0);
    let sound = rig.client.view(start, 0).numbers.sound;
    assert_eq!(
        sound.map(|sound| sound.muted),
        Some(false),
        "someone else's"
    );
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

// Until the host's Hello says what it runs, nothing says the host is too old
// for PC sound: every new session waits for it again, and the panel's switch
// would show the old-host line meanwhile. This rig's host says the version
// of this build first, older than PC_SOUND_SINCE until "move to 1.3.0", and
// 1.2.0 on the second session.
#[test]
fn the_old_host_line_waits_for_the_hosts_hello() {
    let start = Instant::now();
    let rig = Rig::new(Timers::default(), start);
    assert_eq!(rig.client.share_view().own_sound.unavailable, None);
    let (mut rig, _) = Rig::joined(rig, start);
    let older = invite::VERSION < peer::PC_SOUND_SINCE;
    let hello_says = older.then_some(crate::view::NoSound::OldHost);
    assert_eq!(rig.client.share_view().own_sound.unavailable, hello_says);

    // Asleep long enough to join again at the wake: a new session, on which
    // the host has said nothing yet.
    let woke = start + Duration::from_secs(20);
    assert!(rig.client.slept(Duration::from_secs(20), woke, &rig.socket));
    rig.tick(woke);
    let (_, mut session, response) = rig.host.answer_all(woke).pop().expect("a try");
    rig.deliver(&response, woke);
    assert!(rig.host.open_all(&mut session) > 0);
    assert_eq!(
        rig.client.share_view().own_sound.unavailable,
        None,
        "a new session"
    );
    rig.host.reliable = Reliable::new();
    let hello = Message::Hello {
        version: Version {
            major: 1,
            minor: 2,
            patch: 0,
        },
        name: String::from("Mara"),
        reached: None,
    };
    let hello = rig.host.say(&mut session, &hello, woke);
    rig.deliver(&hello, woke);
    assert_eq!(
        rig.client.share_view().own_sound.unavailable,
        Some(crate::view::NoSound::OldHost)
    );
}

// A new session forgets what the sharer said on the old one: the host forgot
// the watch, which this PC asks for again, and says the word again in its
// answer to that Watch; a word said meanwhile went with the old session.
#[test]
fn a_new_session_forgets_the_sharers_word() {
    let start = Instant::now();
    let (mut rig, mut session, _speaker) = watching(peer::PC_SOUND_SINCE, start);
    bo_says(&mut rig, &mut session, true, start);
    assert_eq!(rig.client.view(start, 0).share.watched_sound, Some(true));
    let woke = start + Duration::from_secs(20);
    assert!(rig.client.slept(Duration::from_secs(20), woke, &rig.socket));
    rig.tick(woke);
    let (_, mut session, response) = rig.host.answer_all(woke).pop().expect("a try");
    rig.deliver(&response, woke);
    // A new session starts the stream over, on the host's side too.
    rig.host.reliable = Reliable::new();
    let said = told_host(&mut rig.host, &mut session);
    let watch = |message: &Message| match message {
        Message::Watch { share, on, .. } => *share == SHARE && *on,
        _ => false,
    };
    assert!(said.iter().any(watch), "{said:?}");
    assert_eq!(rig.client.view(woke, 0).share.watched_sound, None);
    let hello = rig.host.hello(&mut session, woke);
    rig.deliver(&hello, woke);
    answer(&mut rig, &mut session, ShareState::Live, false, woke);
    assert_eq!(rig.client.view(woke, 0).share.watched_sound, Some(false));
}

// Bo's off on the old session holds no frame back on the new one: told off
// at the Watch asked for again, as while his on waits at the host, Ana
// hears the frames that come after it though none starts a spell for her.
#[test]
fn an_off_on_the_old_session_holds_no_frame_back() {
    let start = Instant::now();
    let (mut rig, mut session, mut speaker) = watching(peer::PC_SOUND_SINCE, start);
    let frames = stereo_frames(16);
    bo_says(&mut rig, &mut session, true, start);
    let packet = relayed(&mut session, BO_SLOT, SHARE, 0, true, &frames[0]);
    rig.deliver(&packet, start);
    loudest(&mut speaker);
    bo_says(&mut rig, &mut session, false, start);
    assert!(rig.client.view(start, 0).numbers.sound.is_none());
    // Its last 2.5 ms fade out.
    loudest(&mut speaker);

    let woke = start + Duration::from_secs(20);
    assert!(rig.client.slept(Duration::from_secs(20), woke, &rig.socket));
    rig.tick(woke);
    let (_, mut session, response) = rig.host.answer_all(woke).pop().expect("a try");
    rig.deliver(&response, woke);
    assert!(rig.host.open_all(&mut session) > 0);
    // A new session starts the stream over, on the host's side too.
    rig.host.reliable = Reliable::new();
    let hello = rig.host.hello(&mut session, woke);
    rig.deliver(&hello, woke);
    answer(&mut rig, &mut session, ShareState::Live, false, woke);
    let bad = rig.client.drops.bad;
    let mut heard = 0.0f32;
    for seq in 5..16u16 {
        let frame = &frames[usize::from(seq)];
        let packet = relayed(&mut session, BO_SLOT, SHARE, seq, false, frame);
        rig.deliver(&packet, woke);
        heard = heard.max(loudest(&mut speaker));
    }
    assert!(heard > 0.15, "{heard}");
    let view = rig.client.view(woke, 0);
    assert_eq!(view.share.watched_sound, Some(false));
    assert!(view.numbers.sound.is_some());
    assert_eq!(view.numbers.sound_dropped, 0);
    assert_eq!(rig.client.drops.bad, bad);
}

// Words the host told the watch Ana just stopped can come after her next
// Watch: here a pause and an off said while she heard Bo's sound. Neither
// counts before the answer to that Watch, so frames from the middle of a
// spell play, and the answer's off, the first word of the watch, lets no ear
// go. Bo's next word counts.
#[test]
fn a_word_for_the_watch_before_is_not_taken() {
    let start = Instant::now();
    let (mut rig, mut session, mut speaker) = watching(peer::PC_SOUND_SINCE, start);
    let frames = stereo_frames(15);
    let bad = rig.client.drops.bad;
    let mut from_bo = |rig: &mut Rig, session: &mut Session, seq: u16| {
        let frame = &frames[usize::from(seq)];
        let packet = relayed(session, BO_SLOT, SHARE, seq, seq == 0, frame);
        rig.deliver(&packet, start);
        loudest(&mut speaker)
    };
    bo_says(&mut rig, &mut session, true, start);
    let mut heard = 0.0f32;
    for seq in 0..5 {
        heard = heard.max(from_bo(&mut rig, &mut session, seq));
    }
    assert!(heard > 0.1, "{heard}");

    assert!(rig.client.watch(SHARE, false, start, &rig.socket));
    assert!(rig.client.watch(SHARE, true, start, &rig.socket));
    bo_stands(&mut rig, &mut session, ShareState::Paused, start);
    bo_says(&mut rig, &mut session, false, start);
    assert_eq!(words(&rig), (ShareState::Live, None));
    let mut heard = 0.0f32;
    for seq in 5..10 {
        heard = heard.max(from_bo(&mut rig, &mut session, seq));
    }
    assert!(heard > 0.1, "{heard}");
    answer(&mut rig, &mut session, ShareState::Live, false, start);
    assert_eq!(words(&rig), (ShareState::Live, Some(false)));
    for seq in 10..15 {
        let heard = from_bo(&mut rig, &mut session, seq);
        assert!(heard > 0.1, "frame {seq}: {heard}");
    }
    assert_eq!(rig.client.view(start, 0).numbers.sound_dropped, 0);
    assert_eq!(rig.client.drops.bad, bad);
    bo_says(&mut rig, &mut session, true, start);
    assert_eq!(words(&rig), (ShareState::Live, Some(true)));
}

// The host answers Ana's Watches in the order they went, and only the
// answer to her last one counts: one that comes after a Stop, or while a
// later Watch waits for its own, answered a watch she stopped.
#[test]
fn only_the_answer_to_the_last_watch_counts() {
    use crate::view::ShareState::{Away, Live, Paused};
    let start = Instant::now();
    let (mut rig, mut session, _speaker) = watch_pressed(peer::PC_SOUND_SINCE, start);
    assert!(rig.client.watch(SHARE, false, start, &rig.socket));
    answer(&mut rig, &mut session, Away, true, start);
    assert!(rig.client.watch(SHARE, true, start, &rig.socket));
    bo_stands(&mut rig, &mut session, Paused, start);
    assert_eq!(words(&rig), (Live, None));
    answer(&mut rig, &mut session, Paused, false, start);
    assert_eq!(words(&rig), (Paused, Some(false)));

    // Stop and Watch twice inside a round trip: the answer to the first
    // Watch is still on its way when the second goes.
    for on in [false, true, false, true] {
        assert!(rig.client.watch(SHARE, on, start, &rig.socket));
    }
    answer(&mut rig, &mut session, Away, true, start);
    assert_eq!(words(&rig), (Live, None));
    answer(&mut rig, &mut session, Live, false, start);
    assert_eq!(words(&rig), (Live, Some(false)));
    bo_says(&mut rig, &mut session, true, start);
    assert_eq!(words(&rig), (Live, Some(true)));
}

// Bo's share ends before the host reads Ana's Watch, which goes unanswered,
// and he shares again. Her Watch of the new share waits for its own answer,
// not for the one the old share never got.
#[test]
fn a_watch_of_the_next_share_waits_for_its_own_answer() {
    let start = Instant::now();
    let (mut rig, mut session, _speaker) = watch_pressed(peer::PC_SOUND_SINCE, start);
    let next = SHARE + 1;
    bo_shares(&mut rig, &mut session, Some(next), start);
    assert!(!rig.client.share_view().watching);
    assert!(rig.client.watch(next, true, start, &rig.socket));
    let told = Message::WatchAnswer {
        share: next,
        state: ShareState::Paused,
        sound: true,
    };
    let packet = rig.host.say(&mut session, &told, start);
    rig.deliver(&packet, start);
    assert_eq!(words(&rig), (ShareState::Paused, Some(true)));
}

// A press refused, here while Ana watches already, sends no Watch, and the
// Watch that tells the host again what her viewer takes starts no watch.
// Neither is answered, so neither holds Bo's words back.
#[test]
fn a_refused_press_or_an_hevc_re_send_waits_for_nothing() {
    let start = Instant::now();
    let (mut rig, mut session, _speaker) = watching(peer::PC_SOUND_SINCE, start);
    // The host here reads nothing Ana sends, so every Watch stays unacked.
    let watches = |rig: &Rig| {
        rig.client
            .link
            .reliable
            .unacked()
            .filter(|message| {
                matches!(
                    Message::decode(message),
                    Some(Message::Watch {
                        share: SHARE,
                        on: true,
                        ..
                    })
                )
            })
            .count()
    };
    assert_eq!(watches(&rig), 1);
    assert!(!rig.client.watch(SHARE, true, start, &rig.socket));
    rig.client.screen.watching.set_cap(WatchCap {
        height: Some(1080),
        ..WatchCap::default()
    });
    rig.client.screen_work(start, &rig.socket);
    assert_eq!(watches(&rig), 2, "the viewer's Watch again");
    bo_stands(&mut rig, &mut session, ShareState::Paused, start);
    bo_says(&mut rig, &mut session, true, start);
    assert_eq!(words(&rig), (ShareState::Paused, Some(true)));
}

// Only a press that starts a watch shows the share live until the host's
// answer. One refused, as from a panel a step behind while Ana watches
// already, leaves it as Bo last said.
#[test]
fn only_a_press_that_starts_a_watch_shows_the_share_live() {
    use crate::view::ShareState::{Live, Paused};
    let start = Instant::now();
    let (mut rig, mut session, _speaker) = watch_pressed(peer::PC_SOUND_SINCE, start);
    answer(&mut rig, &mut session, Paused, true, start);
    assert!(!rig.client.watch(SHARE, true, start, &rig.socket));
    assert_eq!(words(&rig), (Paused, Some(true)));
    assert!(rig.client.watch(SHARE, false, start, &rig.socket));
    assert!(rig.client.watch(SHARE, true, start, &rig.socket));
    assert_eq!(words(&rig), (Live, None));
    answer(&mut rig, &mut session, Paused, true, start);
    assert_eq!(words(&rig), (Paused, Some(true)));
}

// A new session shows the share live until the host's answer to the Watch
// asked for again says how it stands, and takes no word of it before that
// answer.
#[test]
fn a_new_session_hears_how_the_share_stands() {
    use crate::view::ShareState::{Away, Live, Paused};
    let start = Instant::now();
    let (mut rig, mut session, _speaker) = watch_pressed(peer::PC_SOUND_SINCE, start);
    answer(&mut rig, &mut session, Paused, true, start);
    assert_eq!(words(&rig), (Paused, Some(true)));
    let woke = start + Duration::from_secs(20);
    assert!(rig.client.slept(Duration::from_secs(20), woke, &rig.socket));
    rig.tick(woke);
    let (_, mut session, response) = rig.host.answer_all(woke).pop().expect("a try");
    rig.deliver(&response, woke);
    assert_eq!(words(&rig), (Live, None));
    // A new session starts the stream over, on the host's side too.
    rig.host.reliable = Reliable::new();
    let said = told_host(&mut rig.host, &mut session);
    let asked = |message: &Message| {
        matches!(
            message,
            Message::Watch {
                share: SHARE,
                on: true,
                ..
            }
        )
    };
    assert!(said.iter().any(asked), "{said:?}");
    let hello = rig.host.hello(&mut session, woke);
    rig.deliver(&hello, woke);
    bo_stands(&mut rig, &mut session, Away, woke);
    assert_eq!(words(&rig), (Live, None));
    answer(&mut rig, &mut session, Live, false, woke);
    assert_eq!(words(&rig), (Live, Some(false)));
}

// Ana's PC sleeps with two of her Watches unanswered, after Stop and Watch
// inside a round trip. Their answers went with the old session: the new one
// waits only for the answer to the Watch it asks again, and takes no word of
// the share before it.
#[test]
fn a_new_session_waits_for_one_answer_whatever_was_due() {
    use crate::view::ShareState::{Away, Live, Paused};
    let start = Instant::now();
    let (mut rig, _, _speaker) = watch_pressed(peer::PC_SOUND_SINCE, start);
    assert!(rig.client.watch(SHARE, false, start, &rig.socket));
    assert!(rig.client.watch(SHARE, true, start, &rig.socket));
    let woke = start + Duration::from_secs(20);
    assert!(rig.client.slept(Duration::from_secs(20), woke, &rig.socket));
    rig.tick(woke);
    let (_, mut session, response) = rig.host.answer_all(woke).pop().expect("a try");
    rig.deliver(&response, woke);
    assert!(rig.host.open_all(&mut session) > 0);
    // A new session starts the stream over, on the host's side too.
    rig.host.reliable = Reliable::new();
    let hello = rig.host.hello(&mut session, woke);
    rig.deliver(&hello, woke);
    bo_stands(&mut rig, &mut session, Away, woke);
    assert_eq!(words(&rig), (Live, None));
    answer(&mut rig, &mut session, Paused, true, woke);
    assert_eq!(words(&rig), (Paused, Some(true)));
    bo_says(&mut rig, &mut session, false, woke);
    assert_eq!(words(&rig), (Paused, Some(false)));
}

// Ana's PC sleeps while the host restarts. The new run numbers its shares
// from 1 again, and none has the number she watched yet when the Watch she
// asks again reaches it, so that Watch goes unanswered and the first roster
// ends the watch. Her Watch of the run's own share of that number, once it
// comes, waits only for its own answer.
#[test]
fn a_watch_of_a_share_numbered_again_waits_for_its_own_answer() {
    use crate::view::ShareState::Paused;
    let start = Instant::now();
    let (mut rig, _, _speaker) = watching(peer::PC_SOUND_SINCE, start);
    let woke = start + Duration::from_secs(20);
    assert!(rig.client.slept(Duration::from_secs(20), woke, &rig.socket));
    rig.tick(woke);
    let (_, mut session, response) = rig.host.answer_all(woke).pop().expect("a try");
    rig.deliver(&response, woke);
    assert!(rig.host.open_all(&mut session) > 0);
    // A new session starts the stream over, on the host's side too.
    rig.host.reliable = Reliable::new();
    let hello = rig.host.hello(&mut session, woke);
    rig.deliver(&hello, woke);
    bo_shares(&mut rig, &mut session, None, woke);
    assert_eq!(rig.client.screen.watched, None, "the watch ends");
    bo_shares(&mut rig, &mut session, Some(SHARE), woke);
    assert!(rig.client.watch(SHARE, true, woke, &rig.socket));
    answer(&mut rig, &mut session, Paused, true, woke);
    assert_eq!(words(&rig), (Paused, Some(true)));
    bo_says(&mut rig, &mut session, false, woke);
    assert_eq!(words(&rig), (Paused, Some(false)));
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
