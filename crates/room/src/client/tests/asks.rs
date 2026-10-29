// This PC's asks to share and the host's answers to them. The host answers
// every ShareStart once, in the order they went, the rates the share's
// thread tells as well as the asks, and only the answer to the ask out now
// changes this PC's share.

use super::*;
use crate::control::EntryShare;
use crate::control::ShareAnswer::{self, Busy, Granted, TooSoon};
use crate::screen::{OwnShare, Refusal, ShareNews};

// The host says `message` to Ana. Everything here is said at one time: the
// host here never reads her acks, so asked once its earlier frames are
// 200 ms old it would send one of them again instead.
fn says(rig: &mut Rig, session: &mut Session, message: &Message, now: Instant) {
    let packet = rig.host.say(session, message, now);
    rig.deliver(&packet, now);
}

fn answers(rig: &mut Rig, session: &mut Session, answer: ShareAnswer, now: Instant) {
    says(rig, session, &Message::ShareAnswer(answer), now);
}

// The host's roster: Mara hosts, and Ana shares `share` at 120 fps, or
// nothing.
fn roster(rig: &mut Rig, session: &mut Session, share: Option<u32>, now: Instant) {
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
            entry(
                *rig.client.identity.public(),
                1,
                "Ana",
                share.map(|number| EntryShare { number, fps: 120 }),
            ),
        ],
    });
    says(rig, session, &roster, now);
}

fn own(rig: &Rig) -> OwnShare {
    rig.client.screen.sharing.state()
}

// The share's thread tells the host the rate it runs at, and the person stops
// and asks again before the host's answer to that is back. That answer, a
// grant under the share just stopped, is not the ask's, and the roster that
// ends that share ends nothing here: only the grant after them is the ask's.
#[test]
fn a_rate_told_before_a_stop_is_no_answer_to_the_next_ask() {
    let start = Instant::now();
    let (mut rig, mut session) = Rig::connected(Timers::default(), start);
    assert!(rig.client.share(120, start, &rig.socket));
    answers(&mut rig, &mut session, Granted { share: 3 }, start);
    assert_eq!(
        own(&rig),
        OwnShare::Sharing {
            number: 3,
            fps: 120
        }
    );
    rig.client
        .screen
        .sharing
        .tell(ShareNews::Fps { share: 3, fps: 60 });
    rig.client.screen_work(start, &rig.socket);
    assert!(rig.client.stop_sharing(start, &rig.socket));
    assert!(rig.client.share(120, start, &rig.socket));

    answers(&mut rig, &mut session, Granted { share: 3 }, start);
    assert_eq!(own(&rig), OwnShare::Asking { fps: 120 });
    roster(&mut rig, &mut session, None, start);
    assert_eq!(own(&rig), OwnShare::Asking { fps: 120 });
    answers(&mut rig, &mut session, Granted { share: 4 }, start);
    roster(&mut rig, &mut session, Some(4), start);
    assert_eq!(
        own(&rig),
        OwnShare::Sharing {
            number: 4,
            fps: 120
        }
    );
}

// Stop and Share again before the first ask's answer came: that answer is not
// the second ask's, whatever it says. With nothing due, an ask takes the
// first answer, and an ask taken back takes none.
#[test]
fn an_answer_to_an_ask_taken_back_answers_nothing() {
    let start = Instant::now();
    let (mut rig, mut session) = Rig::connected(Timers::default(), start);
    assert!(rig.client.share(120, start, &rig.socket));
    assert!(rig.client.stop_sharing(start, &rig.socket));
    assert!(rig.client.share(90, start, &rig.socket));
    answers(&mut rig, &mut session, TooSoon, start);
    assert_eq!(own(&rig), OwnShare::Asking { fps: 90 });
    answers(&mut rig, &mut session, Granted { share: 4 }, start);
    assert_eq!(own(&rig), OwnShare::Sharing { number: 4, fps: 90 });

    assert!(rig.client.stop_sharing(start, &rig.socket));
    assert!(rig.client.share(60, start, &rig.socket));
    answers(&mut rig, &mut session, Granted { share: 5 }, start);
    assert_eq!(own(&rig), OwnShare::Sharing { number: 5, fps: 60 });

    // Someone else started sharing before the host read the ask: its answer
    // comes after the Stop, and puts nothing in the chat.
    assert!(rig.client.stop_sharing(start, &rig.socket));
    assert!(rig.client.share(120, start, &rig.socket));
    assert!(rig.client.stop_sharing(start, &rig.socket));
    let busy = Busy {
        key: [9; 32],
        name: String::from("Bo"),
    };
    answers(&mut rig, &mut session, busy, start);
    assert_eq!(own(&rig), OwnShare::Off);
    let view = rig.client.view(start, 0);
    let lines: Vec<&str> = view
        .chat
        .iter()
        .filter(|line| line.kind == LineKind::System)
        .map(|line| line.text.as_str())
        .collect();
    assert!(lines.is_empty(), "{lines:?}");

    // That answer came while nothing was asked, and was counted all the
    // same: the next ask takes the next answer.
    assert!(rig.client.share(60, start, &rig.socket));
    answers(&mut rig, &mut session, Granted { share: 6 }, start);
    assert_eq!(own(&rig), OwnShare::Sharing { number: 6, fps: 60 });
}

// While this PC shares, every answer is to a rate its share's thread told,
// and changes nothing, whatever it says; each still counts, so the next ask
// after a Stop takes its own answer.
#[test]
fn an_answer_while_sharing_changes_nothing() {
    let start = Instant::now();
    let (mut rig, mut session) = Rig::connected(Timers::default(), start);
    assert!(rig.client.share(120, start, &rig.socket));
    answers(&mut rig, &mut session, Granted { share: 3 }, start);
    let sharing = OwnShare::Sharing {
        number: 3,
        fps: 120,
    };
    assert_eq!(own(&rig), sharing);
    let busy = Busy {
        key: [9; 32],
        name: String::from("Bo"),
    };
    for (fps, answer) in [(60, TooSoon), (90, busy), (100, Granted { share: 7 })] {
        rig.client
            .screen
            .sharing
            .tell(ShareNews::Fps { share: 3, fps });
        rig.client.screen_work(start, &rig.socket);
        answers(&mut rig, &mut session, answer, start);
        assert_eq!(own(&rig), sharing, "after the answer to {fps} fps");
    }
    let view = rig.client.view(start, 0);
    assert!(
        view.chat.iter().all(|line| line.kind != LineKind::System),
        "a system line"
    );

    assert!(rig.client.stop_sharing(start, &rig.socket));
    assert!(rig.client.share(120, start, &rig.socket));
    answers(&mut rig, &mut session, Granted { share: 4 }, start);
    assert_eq!(
        own(&rig),
        OwnShare::Sharing {
            number: 4,
            fps: 120
        }
    );
}

// An ask the old session never answered: its answer went with that stream,
// so the new session's first ask takes the first answer.
#[test]
fn a_new_session_waits_for_no_answer_from_the_old_one() {
    let start = Instant::now();
    let (mut rig, _session) = Rig::connected(Timers::default(), start);
    assert!(rig.client.share(120, start, &rig.socket));

    // Ana's PC sleeps 20 s, long enough to join again at the wake on a new
    // session, which starts the host's stream over too.
    let woke = start + Duration::from_secs(20);
    assert!(rig.client.slept(Duration::from_secs(20), woke, &rig.socket));
    rig.tick(woke);
    let (_, mut session, response) = rig.host.answer_all(woke).pop().expect("a try");
    rig.deliver(&response, woke);
    assert!(rig.host.open_all(&mut session) > 0);
    rig.host.reliable = Reliable::new();
    let hello = rig.host.hello(&mut session, woke);
    rig.deliver(&hello, woke);
    assert_eq!(rig.client.state, LinkState::Live);
    assert_eq!(own(&rig), OwnShare::Refused(Refusal::NotLive));

    assert!(rig.client.share(120, woke, &rig.socket));
    answers(&mut rig, &mut session, Granted { share: 9 }, woke);
    assert_eq!(
        own(&rig),
        OwnShare::Sharing {
            number: 9,
            fps: 120
        }
    );
}
