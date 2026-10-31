// Sharing through the room: who shares, who watches, the video and pointer
// packets the host passes on, and what goes back to the sharer.
// Everything runs on loopback with the fake audio devices and no GPU: the
// sharer is played through the Outbox hook with packets channels::video's
// packetizer makes from made-up bytes, and the viewer through the Watching
// hook. Nothing captures, encodes, decodes or shows a picture.

mod common;

use std::net::UdpSocket;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use channels::Channel;
use channels::video::{FrameFacts, Packetizer};
use common::link::Link;
use common::{
    Folder, Forwarder, Hand, Member, code_to_invite, config_in, fresh_log, loopback, poll,
    read_log, timers,
};
use invite::Invite;
use room::screen::{
    Answer, Back, Batch, OwnShare, Pointer, Refusal, Shape, ShapeKind, Sharing, Video, WatchEvent,
    Watching,
};
use room::view::{LineKind, LinkState, ShareState, Source, View};
use room::{Config, Room, Timers};

// What a video packet may carry on a 1200-byte datagram.
const PAYLOAD: usize = 1200 - 32 - 1 - 2;
const WAIT: Duration = Duration::from_secs(3);

// A host and friends who joined with one multi-use invite, all live and in
// everyone's roster.
fn room_of(names: &[&str], timers: Timers) -> (Member, Vec<Member>, Invite) {
    let host = Member::host(names[0], timers);
    host.room().new_invite(true);
    let view = host.wait_for(WAIT, "a multi-use invite", |v| {
        v.invite
            .as_ref()
            .is_some_and(|i| i.multi_use && !i.code.is_empty())
    });
    let invite = code_to_invite(&view.invite.expect("an invite").code, loopback(host.port()));
    let friends: Vec<Member> = names[1..]
        .iter()
        .map(|name| Member::join(name, timers, invite.clone()))
        .collect();
    let people = names.len();
    for friend in &friends {
        friend.wait_for(WAIT, "live with everyone", |v| {
            v.strip.state == LinkState::Live && v.people.len() == people
        });
    }
    host.wait_for(WAIT, "everyone in", |v| v.people.len() == people);
    (host, friends, invite)
}

fn number(view: &View) -> Option<u32> {
    match view.share.own {
        OwnShare::Sharing { number, .. } => Some(number),
        _ => None,
    }
}

fn shares(member: &Member, fps: u8) -> u32 {
    member.room().share(fps, None);
    let view = member.wait_for(WAIT, "the share granted", |v| number(v).is_some());
    number(&view).expect("granted")
}

fn sees(member: &Member, share: u32) {
    member.wait_for(WAIT, "the share in the roster", |v| {
        v.share.current.as_ref().is_some_and(|c| c.number == share)
    });
}

fn watches(member: &Member, share: u32) {
    sees(member, share);
    member.room().watch(share, true);
    member.wait_for(WAIT, "watching", |v| v.share.watching);
}

fn system_lines(view: &View) -> Vec<String> {
    view.chat
        .iter()
        .filter(|line| line.kind == LineKind::System)
        .map(|line| line.text.clone())
        .collect()
}

// One frame's packets from the packetizer: `len` made-up bytes of access
// unit, stamped as captured at `captured` on the sharer's clock.
fn frame(number: u32, len: usize, captured: u64, payload: usize) -> Vec<Vec<u8>> {
    let facts = FrameFacts {
        number,
        idr: number == 0,
        survives_loss: true,
        hevc: false,
        captured,
        encoded: captured + 2500,
    };
    let unit: Vec<u8> = (0..len).map(|i| (i as u32 ^ number) as u8).collect();
    let mut packetizer = Packetizer::new(payload).expect("a packetizer");
    let packets = packetizer.packetize(&facts, &unit, 20).expect("packetized");
    packets.iter().map(<[u8]>::to_vec).collect()
}

// Everything that reaches a viewer within `within`, until it has `want`
// packets.
struct Seen {
    video: Vec<Video>,
    pointer: Option<(u32, Pointer)>,
    shapes: Vec<(u32, u32, Shape)>,
    events: Vec<WatchEvent>,
}

fn seen(watching: &Watching, want: usize, within: Duration) -> Seen {
    let deadline = Instant::now() + within;
    let mut batch = Batch::default();
    let mut out = Seen {
        video: Vec::new(),
        pointer: None,
        shapes: Vec::new(),
        events: Vec::new(),
    };
    loop {
        watching.take(&mut batch);
        out.video.extend(batch.video.drain(..));
        out.pointer = batch.pointer.or(out.pointer);
        out.shapes.append(&mut batch.shapes);
        out.events.append(&mut batch.events);
        if out.video.len() >= want || Instant::now() >= deadline {
            return out;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn answers_within(
    sharing: &Sharing,
    within: Duration,
    done: impl Fn(&[Answer]) -> bool,
) -> Vec<Answer> {
    let deadline = Instant::now() + within;
    let mut got = Vec::new();
    loop {
        got.extend(sharing.take_answers());
        if done(&got) || Instant::now() >= deadline {
            return got;
        }
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn one_share_at_a_time() {
    let (host, friends, _) = room_of(&["Mara", "Ana", "Bo"], timers());
    let (ana, bo) = (&friends[0], &friends[1]);
    assert_eq!(host.view().share.own, OwnShare::Off);

    let share = shares(ana, 120);
    for member in [&host, bo] {
        let view = member.wait_for(WAIT, "Ana sharing", |v| {
            v.people.iter().any(|p| p.name == "Ana" && p.sharing)
                && system_lines(v).contains(&String::from("Ana started sharing"))
        });
        let current = view.share.current.expect("a share");
        assert_eq!(
            (current.name.as_str(), current.number, current.fps),
            ("Ana", share, 120)
        );
        assert!(!current.yours && !view.share.watching);
    }
    // The grant and the roster that shows the share are two messages, and
    // the roster reaching the host's and Bo's views first says nothing
    // about when it reaches Ana's.
    let own = ana.wait_for(WAIT, "Ana's roster showing her share", |v| {
        v.share.current.as_ref().is_some_and(|c| c.yours)
    });
    assert!(
        system_lines(&own).is_empty(),
        "no line about one's own share"
    );

    bo.room().share(60, None);
    let refused = bo.wait_for(WAIT, "Bo refused", |v| {
        matches!(v.share.own, OwnShare::Refused(_))
    });
    let OwnShare::Refused(why) = &refused.share.own else {
        unreachable!()
    };
    assert_eq!(
        *why,
        Refusal::Busy {
            name: String::from("Ana")
        }
    );
    assert_eq!(why.sentence(), "Ana is sharing. One share at a time.");
    assert!(system_lines(&refused).contains(&why.sentence()));

    // The host is one of us, and waits its turn too.
    host.room().share(60, None);
    let refused = host.wait_for(WAIT, "the host refused", |v| {
        matches!(v.share.own, OwnShare::Refused(_))
    });
    assert_eq!(
        refused.share.own,
        OwnShare::Refused(Refusal::Busy {
            name: String::from("Ana")
        })
    );
    assert_eq!(
        refused.share.current.map(|c| (c.name, c.number)),
        Some((String::from("Ana"), share))
    );

    // Stopping needs no answer, and the host may go then.
    ana.room().stop_sharing();
    for member in [&host, bo] {
        member.wait_for(WAIT, "Ana stopped", |v| {
            v.share.current.is_none()
                && system_lines(v).contains(&String::from("Ana stopped sharing"))
        });
    }
    let hosts = shares(&host, 120);
    assert_ne!(hosts, share, "a new share gets a new number");
    bo.room().share(60, None);
    // The answer can come before the roster that shows the share, which
    // waits out its 250 ms gap.
    bo.wait_for(WAIT, "Bo refused again, and Mara sharing", |v| {
        v.share.own
            == OwnShare::Refused(Refusal::Busy {
                name: String::from("Mara"),
            })
            && v.people.iter().any(|p| p.name == "Mara" && p.sharing)
    });
    println!(
        "share {share} was Ana's; Bo and the host were refused naming her; then the host's share {hosts}, and Bo was refused naming Mara"
    );
}

#[test]
fn share_ends_when_its_sharer_leaves_or_is_lost() {
    let (host, mut friends, invite) = room_of(&["Mara", "Ana", "Bo"], timers());
    let share = shares(&friends[0], 120);
    watches(&friends[1], share);
    watches(&host, share);
    friends[0].leave();
    for (who, member) in [("Bo", &friends[1]), ("the host", &host)] {
        let view = member.wait_for(WAIT, "the share over", |v| {
            v.share.current.is_none() && !v.share.watching
        });
        assert!(
            system_lines(&view).contains(&String::from("Ana stopped sharing")),
            "{who}: {:?}",
            system_lines(&view)
        );
        let events = seen(&member.room().watching(), 0, Duration::ZERO).events;
        assert_eq!(
            events,
            [
                WatchEvent::Started {
                    share,
                    fps: 120,
                    name: String::from("Ana")
                },
                WatchEvent::Ended { share }
            ],
            "{who}"
        );
    }

    // Cy behind a forwarder that goes silent: lost after 3 s here.
    let forwarder = Forwarder::new(loopback(host.port()));
    let mut through = invite.clone();
    through.candidates = vec![invite::Candidate {
        kind: invite::CandidateKind::Lan,
        addr: forwarder.addr,
    }];
    let cy = Member::join("Cy", timers(), through);
    cy.wait_for(WAIT, "Cy live", |v| v.strip.state == LinkState::Live);
    let second = shares(&cy, 60);
    assert_ne!(second, share);
    let bo = &friends[1];
    watches(bo, second);
    forwarder.block(true);
    let blocked = Instant::now();
    let view = bo.wait_for(Duration::from_secs(6), "Cy's share over", |v| {
        v.share.current.is_none()
    });
    println!(
        "Ana left and both watchers saw her share end; Cy's ended {:.1} s after her link went silent",
        blocked.elapsed().as_secs_f32()
    );
    assert!(system_lines(&view).contains(&String::from("Cy stopped sharing")));
    let events = seen(&bo.room().watching(), 0, Duration::ZERO).events;
    assert_eq!(events.last(), Some(&WatchEvent::Ended { share: second }));
    friends.clear();
}

// Ana stops and asks again before the answer to her first ask is back. The
// host grants the first ask, ends that share at the stop and grants the
// second, and the roster that ends the first share reaches her between the
// two grants. She shares under the second number, and stays on it.
#[test]
fn an_ask_taken_back_and_made_again_shares_under_the_new_number() {
    let (host, _, mut through) = room_of(&["Mara"], timers());
    let forwarder = Forwarder::new(loopback(host.port()));
    through.candidates = vec![invite::Candidate {
        kind: invite::CandidateKind::Lan,
        addr: forwarder.addr,
    }];
    let ana = Member::join("Ana", timers(), through);
    ana.wait_for(WAIT, "Ana live", |v| v.strip.state == LinkState::Live);
    // Everything Ana sends reaches the host 600 ms late. Her stop and second
    // ask leave 400 ms after the first ask, 200 ms before its answer is back,
    // and reach the host 400 ms after the roster that shows the first share,
    // 150 ms past the 250 ms the host leaves between two rosters, so the one
    // that ends the first share goes at once, ahead of the second grant. If
    // the times slip the test still passes, but may not meet that order.
    forwarder.hold(Duration::from_millis(600), Duration::from_millis(600));
    ana.room().share(60, None);
    thread::sleep(Duration::from_millis(400));
    ana.room().stop_sharing();
    ana.room().share(60, None);
    let view = host.wait_for(WAIT, "Ana's second share at the host", |v| {
        let started = system_lines(v)
            .iter()
            .filter(|line| *line == "Ana started sharing")
            .count();
        v.share.current.as_ref().is_some_and(|c| c.name == "Ana") && started == 2
    });
    let second = view.share.current.expect("a share").number;
    ana.wait_for(WAIT, "Ana sharing under the second number", |v| {
        number(v) == Some(second)
    });
    ana.holds_for(
        Duration::from_millis(500),
        "Ana left the second share",
        |v| number(v) == Some(second),
    );
}

#[test]
fn video_reaches_only_the_watchers() {
    let (host, friends, invite) = room_of(&["Mara", "Ana", "Bo", "Cy"], timers());
    let (ana, bo, cy) = (&friends[0], &friends[1], &friends[2]);
    let share = shares(ana, 120);
    watches(bo, share);
    sees(cy, share);
    let sharing = ana.room().sharing();
    let facts = poll(WAIT, "facts from the host", || {
        sharing.facts().filter(|facts| facts.watchers == 1)
    });
    assert_eq!(
        (facts.internet, facts.rate_kbps, facts.lan),
        (0, 15_000, true)
    );
    assert_eq!(facts.payload(), 1400 - 32 - 1 - 2);

    let mut outbox = sharing.outbox();
    let sent: Vec<Vec<u8>> = (0..4u32)
        .flat_map(|n| frame(n, 6000 + n as usize * 1000, 1_000 + u64::from(n), PAYLOAD))
        .collect();
    for packet in &sent {
        assert_eq!(outbox.video(packet), 1, "one link, the host's");
    }
    let got = seen(&bo.room().watching(), sent.len(), WAIT);
    assert_eq!(got.video.len(), sent.len());
    assert!(
        got.video
            .iter()
            .zip(&sent)
            .all(|(video, packet)| video.share == share && video.packet == *packet)
    );
    for (who, member) in [("Cy", cy), ("Ana", ana), ("the host", &host)] {
        let nothing = seen(&member.room().watching(), 1, Duration::from_millis(200));
        assert!(nothing.video.is_empty(), "{who} got video without watching");
    }
    // Video changes nothing the panel shows, so the numbers catch up with
    // the next view.
    let numbers = host
        .wait_for(WAIT, "the relayed count", |v| {
            v.numbers.video_relayed == sent.len() as u64
        })
        .numbers;
    ana.wait_for(WAIT, "the sent count", |v| {
        v.numbers.video_sent == sent.len() as u64
    });

    // The host watching too gets the next frame from its own receive thread.
    watches(&host, share);
    let next = frame(9, 3000, 5_000, PAYLOAD);
    for packet in &next {
        outbox.video(packet);
    }
    for member in [&host, bo] {
        let got = seen(&member.room().watching(), next.len(), WAIT);
        assert_eq!(
            got.video.iter().map(|v| &v.packet).collect::<Vec<_>>(),
            next.iter().collect::<Vec<_>>()
        );
    }

    // A pointer goes the same way, newest wins, and its shape over the
    // control channel.
    let arrow = Shape {
        kind: ShapeKind::Color,
        width: 48,
        height: 48,
        pitch: 192,
        hotspot_x: 2,
        hotspot_y: 3,
        scale_milli: 667,
        bytes: (0..48 * 192).map(|i| i as u8).collect(),
    };
    let id = sharing.shape(arrow.clone()).expect("a shape Booth sends");
    for x in 0..5 {
        outbox.pointer(100 + x, 200, true, id, false);
    }
    for member in [&host, bo] {
        // Each look takes what has come, and the five pointers and the shape
        // can come over several looks, so what came is kept until the last
        // pointer and the shape are both here.
        let mut got = seen(&member.room().watching(), 0, Duration::ZERO);
        poll(WAIT, "the last pointer and the shape", || {
            let more = seen(&member.room().watching(), 0, Duration::ZERO);
            got.pointer = more.pointer.or(got.pointer.take());
            got.shapes.extend(more.shapes);
            let last = got
                .pointer
                .as_ref()
                .is_some_and(|(_, pointer)| pointer.x == 104);
            (last && !got.shapes.is_empty()).then_some(())
        });
        let (at, pointer) = got.pointer.expect("a pointer");
        assert_eq!(
            (at, pointer.x, pointer.y, pointer.shape),
            (share, 104, 200, id)
        );
        assert_eq!(got.shapes, [(share, id, arrow.clone())]);
    }

    // The largest shape fits what is left of Ana's budget and goes at once.
    // One more right behind it does not: it waits for the budget, where
    // before it went and the host dropped it whole. The budget holds 272 KB
    // and fills at 56 KB a second, so the second leaves no sooner than
    // 256 + 64 - 272 = 48 KB later, 857 ms after the first.
    let color = |side: u16| Shape {
        kind: ShapeKind::Color,
        width: side,
        height: side,
        pitch: side * 4,
        hotspot_x: 0,
        hotspot_y: 0,
        scale_milli: 1000,
        bytes: (0..usize::from(side) * usize::from(side) * 4)
            .map(|i| (i / 7) as u8)
            .collect(),
    };
    let reached = |id: u32, within: Duration| {
        poll(within, "a shape at Bo's", || {
            seen(&bo.room().watching(), 0, Duration::ZERO)
                .shapes
                .into_iter()
                .find(|(_, got, _)| *got == id)
        })
    };
    let largest = color(256);
    let first = Instant::now();
    let id = sharing.shape(largest.clone()).expect("a shape Booth sends");
    assert_eq!(reached(id, WAIT), (share, id, largest));
    let next = color(128);
    let id = sharing.shape(next.clone()).expect("a shape Booth sends");
    assert_eq!(reached(id, WAIT), (share, id, next));
    let apart = first.elapsed();
    println!(
        "a 64 KB shape right behind a 256 KB one reached Bo whole {:.0} ms after the first was set",
        apart.as_secs_f64() * 1000.0
    );
    assert!(apart > Duration::from_millis(850), "{apart:?}");
    assert_eq!(host.view().numbers.video_dropped, 0);

    // What Bo got from the host, read by hand: the host's own prefix with
    // Ana's slot, and the packet byte for byte.
    let mut eve = Hand::join(&invite, loopback(host.port()), "Eve");
    sees(&host, share);
    let mut watch = vec![12];
    watch.extend_from_slice(&share.to_le_bytes());
    // On, and its viewer decodes HEVC.
    watch.extend_from_slice(&[1, 1]);
    eve.say(&watch);
    host.wait_for(WAIT, "Eve watching", |v| {
        v.share
            .current
            .as_ref()
            .is_some_and(|c| c.watchers == Some(3))
    });
    let last = frame(10, 2000, 6_000, PAYLOAD);
    for packet in &last {
        outbox.video(packet);
    }
    // Kept across looks: the copies can come in over more than one.
    let mut video: Vec<Vec<u8>> = Vec::new();
    let ana_slot = poll(WAIT, "Eve's copies", || {
        video.extend(
            eve.received()
                .into_iter()
                .filter(|(channel, _)| *channel == Channel::Video)
                .map(|(_, payload)| payload),
        );
        (video.len() >= last.len()).then(|| video.clone())
    });
    for (relayed, packet) in ana_slot.iter().zip(&last) {
        assert_eq!(relayed[0], 2, "Relayed");
        assert_ne!(relayed[1], 0, "Ana's slot, not the host's");
        assert_eq!(&relayed[2..], &packet[..]);
    }
    println!(
        "{} packets from Ana reached Bo byte for byte and nobody who was not watching; the host relayed {}; Eve saw Relayed from slot {}",
        sent.len(),
        numbers.video_relayed,
        ana_slot[0][1]
    );
}

#[test]
fn watcher_asks_reach_the_sharer() {
    // Reports once in 10 s, so only what goes at once can arrive in the
    // test's time.
    let timers = Timers {
        voice_report_every: Duration::from_secs(10),
        ..timers()
    };
    let (host, friends, _) = room_of(&["Mara", "Ana", "Bo", "Cy"], timers);
    let (ana, bo, cy) = (&friends[0], &friends[1], &friends[2]);
    let share = shares(ana, 120);
    let sharing = ana.room().sharing();
    watches(bo, share);
    let started = Instant::now();
    let got = answers_within(&sharing, WAIT, |a| a.contains(&Answer::Idr { seen: None }));
    println!(
        "the IDR ask for a new watcher reached Ana {:.1} ms after Bo pressed Watch",
        started.elapsed().as_secs_f64() * 1000.0
    );
    assert_eq!(got, [Answer::Idr { seen: None }]);
    watches(cy, share);
    assert_eq!(
        answers_within(&sharing, WAIT, |a| !a.is_empty()),
        [Answer::Idr { seen: None }]
    );

    // A recover request is about a frame that went out: frames 0 to 29 do.
    let sent: Vec<Vec<u8>> = (0..30u32)
        .flat_map(|n| frame(n, 1000, 0, PAYLOAD))
        .collect();
    let mut outbox = sharing.outbox();
    for packet in &sent {
        outbox.video(packet);
    }
    let (bo_view, cy_view) = (bo.room().watching(), cy.room().watching());
    for watching in [&bo_view, &cy_view] {
        assert_eq!(seen(watching, sent.len(), WAIT).video.len(), sent.len());
    }

    // Bo's range goes on; Cy's overlap only adds what Bo's did not cover,
    // and a range already covered goes nowhere.
    bo_view.back(Back::Recover {
        share,
        first: 10,
        last: 12,
    });
    assert_eq!(
        answers_within(&sharing, WAIT, |a| !a.is_empty()),
        [Answer::Recover {
            first: 10,
            last: 12
        }]
    );
    cy_view.back(Back::Recover {
        share,
        first: 10,
        last: 14,
    });
    cy_view.back(Back::Recover {
        share,
        first: 11,
        last: 12,
    });
    assert_eq!(
        answers_within(&sharing, Duration::from_millis(300), |_| false),
        [Answer::Recover {
            first: 13,
            last: 14
        }]
    );
    // A range for a share that is not on goes nowhere either, and one about
    // frames that have not gone out yet counts as bad.
    bo_view.back(Back::Recover {
        share: share.wrapping_add(1).max(1),
        first: 50,
        last: 50,
    });
    let bad = host.view().numbers.dropped_bad;
    bo_view.back(Back::Recover {
        share,
        first: 29,
        last: 31,
    });
    host.wait_for(WAIT, "the range ahead counted as bad", |v| {
        v.numbers.dropped_bad > bad
    });

    // An IDR ask from one watcher goes at most every half second, its Watch
    // included.
    thread::sleep(Duration::from_millis(600));
    for seen in 40..45 {
        bo_view.back(Back::Idr { share, seen });
    }
    assert_eq!(
        answers_within(&sharing, Duration::from_millis(300), |_| false),
        [Answer::Idr { seen: Some(40) }]
    );

    // The viewer paces its loss reports itself, so each one reaches the host
    // at once. The host passes a clean one on at its once-a-second tick,
    // which is 10 s away here, and the first nonzero one at once.
    bo_view.back(Back::Loss {
        share,
        loss: Some(0.0),
    });
    assert!(answers_within(&sharing, Duration::from_millis(300), |_| false).is_empty());
    let reported = Instant::now();
    cy_view.back(Back::Loss {
        share,
        loss: Some(3.5),
    });
    let got = answers_within(&sharing, Duration::from_secs(1), |a| !a.is_empty());
    let took = reported.elapsed();
    println!(
        "Cy's first nonzero loss reached Ana {:.1} ms after the viewer handed it over",
        took.as_secs_f64() * 1000.0
    );
    assert_eq!(got, [Answer::Loss(Some(3.5))]);
    assert!(took < Duration::from_millis(250), "{took:?}");
    // Cy's clean report reaches the host at once too and brings the worst
    // back to nothing, so the nonzero one right behind it is a first again
    // and goes at once. A client that held its reports for its own tick
    // would have kept the host at 3.5 and this one back 10 s.
    cy_view.back(Back::Loss {
        share,
        loss: Some(0.0),
    });
    cy_view.back(Back::Loss {
        share,
        loss: Some(2.0),
    });
    assert_eq!(
        answers_within(&sharing, Duration::from_secs(1), |a| !a.is_empty()),
        [Answer::Loss(Some(2.0))]
    );
    let dropped = host.view().numbers.video_dropped;
    assert_eq!(dropped, 4, "the IDR asks held back");
}

// The rule from the host's side, faked on one PC: every link is on
// loopback, which counts as the LAN, so the internet half is in the unit
// tests where a path can be set.
#[test]
fn rate_facts_follow_the_watchers() {
    let (host, friends, _) = room_of(&["Mara", "Ana", "Bo"], timers());
    let share = shares(&host, 120);
    let sharing = host.room().sharing();
    let facts = poll(WAIT, "facts", || sharing.facts());
    assert_eq!(
        (
            facts.watchers,
            facts.internet,
            facts.rate_kbps,
            facts.spread,
            facts.lan
        ),
        (0, 0, 15_000, false, true)
    );
    for (i, friend) in friends.iter().enumerate() {
        watches(friend, share);
        poll(WAIT, "a watcher more", || {
            sharing.facts().filter(|f| usize::from(f.watchers) == i + 1)
        });
    }
    friends[0].room().watch(share, false);
    let facts = poll(WAIT, "a watcher less", || {
        sharing.facts().filter(|f| f.watchers == 1)
    });
    assert_eq!(
        (facts.internet, facts.rate_kbps, facts.lan),
        (0, 15_000, true)
    );
    assert_eq!(host.view().numbers.share_rate_kbps, Some(15_000));
}

// The facts the sharer hears say whether every watcher decodes HEVC, from
// what each one's Watch said. A friend whose viewer does not makes them say
// H.264 for as long as they watch.
#[test]
fn hevc_only_while_every_watcher_decodes_it() {
    let (host, friends, invite) = room_of(&["Mara", "Ana"], timers());
    let mut config = common::config("Bo", timers());
    config.video.hevc = false;
    let bo = Member::join_with(config, Arc::new(keys::Identity::generate()), invite);
    bo.wait_for(WAIT, "live with everyone", |v| {
        v.strip.state == LinkState::Live && v.people.len() == 3
    });
    host.wait_for(WAIT, "everyone in", |v| v.people.len() == 3);
    let share = shares(&host, 120);
    let sharing = host.room().sharing();
    let facts_with = |watchers: u8, what: &str| {
        poll(WAIT, what, || {
            sharing.facts().filter(|f| f.watchers == watchers)
        })
    };
    assert!(facts_with(0, "facts").hevc, "nobody watching");
    watches(&friends[0], share);
    assert!(facts_with(1, "Ana watching").hevc);
    watches(&bo, share);
    assert!(!facts_with(2, "Bo watching").hevc);
    bo.room().watch(share, false);
    assert!(facts_with(1, "Bo gone").hevc);
}

// A watcher turns the sharer's capture time into its own clock through its
// own offset to the host and the one the host gives it, or its own alone
// when the host shares or watches.
#[test]
fn capture_time_on_the_watchers_clock() {
    let (host, friends, _) = room_of(&["Mara", "Ana", "Bo"], timers());
    let (ana, bo) = (&friends[0], &friends[1]);
    let share = shares(ana, 120);
    watches(bo, share);
    watches(&host, share);
    let mut apart = Vec::new();
    for (who, member) in [("Bo, through the host", bo), ("the host", &host)] {
        let watching = member.room().watching();
        poll(WAIT, "an offset", || watching.to_here(share, 0));
        let at = Instant::now();
        let captured = ana.room().sharing().micros(at);
        let (here, about) = watching.to_here(share, captured).expect("an offset");
        let off = here as i64 - watching.micros(at) as i64;
        println!(
            "{who}: Ana's capture time lands {:.2} ms off this PC's own clock{}",
            off as f64 / 1000.0,
            if about { ", about" } else { "" }
        );
        apart.push(off);
    }
    ana.room().stop_sharing();
    bo.wait_for(WAIT, "Ana stopped", |v| v.share.current.is_none());
    let hosts = shares(&host, 60);
    watches(bo, hosts);
    let watching = bo.room().watching();
    poll(WAIT, "an offset to the host", || watching.to_here(hosts, 0));
    let at = Instant::now();
    let (here, _) = watching
        .to_here(hosts, host.room().sharing().micros(at))
        .expect("an offset");
    let off = here as i64 - watching.micros(at) as i64;
    println!(
        "Bo, the host's own share: {:.2} ms off",
        off as f64 / 1000.0
    );
    apart.push(off);
    // The tolerance the voice tests hold mouth to ear to.
    assert!(apart.iter().all(|off| off.abs() < 2000), "{apart:?}");
    assert_eq!(
        watching.to_here(hosts.wrapping_add(1).max(1), 0),
        None,
        "another share has no offset"
    );
}

fn with_prefix(kind: u8, packet: &[u8]) -> Vec<u8> {
    let mut out = vec![kind, 0];
    out.extend_from_slice(packet);
    out
}

// A friend's program can send as fast as its link goes. Pointer updates
// past their burst and rate are dropped and counted. Video has the same
// kind of limit, sized for 80 Mbit/s and the IDRs a watcher can add, which
// a debug build's host on one PC cannot receive fast enough to reach; the
// host's unit tests hold it to its numbers with a made-up clock.
#[test]
fn pointer_flood_is_limited() {
    let (host, friends, invite) = room_of(&["Mara", "Bo"], timers());
    let bo = &friends[0];
    let mut eve = Hand::join(&invite, loopback(host.port()), "Eve");
    eve.say(&[9, 120]);
    let view = host.wait_for(WAIT, "Eve sharing", |v| {
        v.share.current.as_ref().is_some_and(|c| c.name == "Eve")
    });
    let share = view.share.current.expect("a share").number;
    watches(bo, share);

    let started = Instant::now();
    for seq in 0..1000u32 {
        let mut pointer = vec![1, 0];
        pointer.extend_from_slice(&seq.to_le_bytes());
        pointer.extend_from_slice(&[0; 9]);
        pointer.extend_from_slice(&0u32.to_le_bytes());
        eve.send(Channel::Cursor, &pointer);
    }
    let numbers = host
        .wait_for(WAIT, "every update counted", |v| {
            v.numbers.video_relayed + v.numbers.video_dropped >= 1000
        })
        .numbers;
    // The host took them no later than it counted them all.
    let took = started.elapsed().as_secs_f64();
    let most = 120.0 + 480.0 * took;
    println!(
        "1000 pointer updates from Eve: {} passed on, {} dropped; {most:.0} may pass in the {:.0} ms it took",
        numbers.video_relayed,
        numbers.video_dropped,
        took * 1000.0
    );
    assert_eq!(numbers.video_relayed + numbers.video_dropped, 1000);
    assert!(numbers.video_dropped > 0);
    assert!(numbers.video_relayed as f64 <= most + 1.0);
    let got = seen(&bo.room().watching(), 0, Duration::from_millis(200));
    assert!(got.pointer.is_some_and(|(at, _)| at == share));
}

// Video from anyone who does not share, and from someone who is not in the
// room at all, goes nowhere and counts as bad.
#[test]
fn video_from_a_non_sharer_counts_as_bad() {
    let (host, friends, invite) = room_of(&["Mara", "Ana", "Bo"], timers());
    let (ana, bo) = (&friends[0], &friends[1]);
    let share = shares(ana, 120);
    watches(bo, share);
    let mut mallory = Hand::join(&invite, loopback(host.port()), "Mallory");
    host.wait_for(WAIT, "Mallory in", |v| v.people.len() == 4);
    let before = host.view().numbers.dropped_bad;
    let packets = frame(1, 4000, 0, PAYLOAD);
    for packet in &packets {
        mallory.send(Channel::Video, &with_prefix(1, packet));
    }
    let pointer = [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0];
    mallory.send(Channel::Cursor, &pointer);
    let stranger = UdpSocket::bind(loopback(0)).expect("a socket");
    let mut junk = vec![0x15, 0, 0, 0];
    junk.extend_from_slice(&[7; 1200]);
    stranger
        .send_to(&junk, loopback(host.port()))
        .expect("send");
    let bad = packets.len() as u64 + 2;
    let view = host.wait_for(WAIT, "all counted as bad", |v| {
        v.numbers.dropped_bad >= before + bad
    });
    println!(
        "{} packets from Mallory, who does not share, and one from a stranger: {} counted as bad",
        bad - 1,
        view.numbers.dropped_bad - before
    );
    assert_eq!(view.numbers.video_relayed, 0);
    let got = seen(&bo.room().watching(), 1, Duration::from_millis(300));
    assert!(got.video.is_empty() && got.pointer.is_none());
}

// xorshift64*, seeded, so a failure shows up the same way every run.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
}

// Whatever a friend's program sends as video, pointers, input or the control
// messages sharing and remote control added, the host takes none of it as
// anything but what it is, and the room goes on: a share still starts and
// its video still goes to the one watching. Eve shares and controls
// nothing, so her input is refused as bad before it is read (tests/control.rs
// has input from a controller the sharer allowed).
#[test]
fn hostile_share_payloads_break_nothing() {
    let (host, friends, invite) = room_of(&["Mara", "Ana", "Bo"], timers());
    let (ana, bo) = (&friends[0], &friends[1]);
    let mut eve = Hand::join(&invite, loopback(host.port()), "Eve");
    // Eve shares, so the host reads what she sends past the first check.
    eve.say(&[9, 60]);
    let view = host.wait_for(WAIT, "Eve sharing", |v| {
        v.share.current.as_ref().is_some_and(|c| c.name == "Eve")
    });
    let share = view.share.current.expect("a share").number;
    watches(bo, share);
    let good = frame(3, 3000, 0, PAYLOAD);
    let mut random = Random(0x9E37_79B9_7F4A_7C15);
    for round in 0..3000usize {
        let len = (random.next() % 1500) as usize;
        match round % 6 {
            0 => eve.send(Channel::Video, &random.bytes(len)),
            1 => {
                // A real packet with a few bytes changed.
                let mut packet = with_prefix(1, &good[round % good.len()]);
                for _ in 0..3 {
                    let at = (random.next() as usize) % packet.len();
                    packet[at] = random.next() as u8;
                }
                eve.send(Channel::Video, &packet);
            }
            2 => eve.send(Channel::Cursor, &random.bytes(len % 40)),
            3 => {
                let mut message = vec![9 + (random.next() % 15) as u8];
                message.extend(random.bytes(len % 1000));
                eve.say(&message);
            }
            4 => {
                let mut message = vec![18];
                message.extend_from_slice(&share.to_le_bytes());
                message.extend(random.bytes(len % 1000));
                eve.say(&message);
            }
            _ => eve.send(Channel::Input, &random.bytes(len)),
        }
        // Paced, so nothing is lost for want of socket buffer on a debug
        // build's host: Eve's control stream is never sent twice.
        if round % 5 == 4 {
            thread::sleep(Duration::from_millis(1));
        }
    }
    eve.say(&[11]);
    // Ana refuses a share herself while her roster still shows Eve's, and a
    // roster can wait ROSTER_GAP behind the host's own view.
    for member in [&host, ana] {
        member.wait_for(WAIT, "Eve's share over", |v| v.share.current.is_none());
    }
    let alive = shares(ana, 120);
    watches(bo, alive);
    let packets = frame(0, 5000, 0, PAYLOAD);
    let mut outbox = ana.room().sharing().outbox();
    for packet in &packets {
        outbox.video(packet);
    }
    let got = seen(&bo.room().watching(), packets.len(), WAIT);
    let from_ana = got.video.iter().filter(|v| v.share == alive).count();
    println!(
        "3000 hostile packets and messages from Eve; the host counted {} bad and the room went on",
        host.view().numbers.dropped_bad
    );
    assert_eq!(from_ana, packets.len());
    for member in [&host, ana, bo] {
        assert_eq!(member.view().strip.state, LinkState::Live);
    }
}

// Video counts as media flowing, both for the sharer, whose own video went
// out lately, and for the link that carried it. The host's strip takes its
// loss and jitter from the video then, since nobody talks.
#[test]
fn video_speeds_up_pings_and_feeds_the_strip() {
    let timers = Timers {
        ping_idle: Duration::from_secs(1),
        ping_media: Duration::from_millis(100),
        reconnecting_after: Duration::from_secs(3),
        lost_after: Duration::from_secs(15),
        ..timers()
    };
    let (host, friends, _) = room_of(&["Mara", "Ana"], timers);
    let ana = &friends[0];
    for member in [ana, &host] {
        member.wait_for(WAIT, "idle pings", |v| {
            v.numbers.ping_interval == Duration::from_secs(1)
        });
    }
    shares(ana, 60);
    let sharing = ana.room().sharing();
    let ana_before = ana.view().strip.trace.len();
    let started = Instant::now();
    let sender = thread::spawn(move || {
        let mut outbox = sharing.outbox();
        let mut next = Instant::now();
        for number in 0..120u32 {
            let captured = sharing.micros(Instant::now());
            for packet in frame(number, 4000, captured, PAYLOAD) {
                outbox.video(&packet);
            }
            next += Duration::from_nanos(16_666_667);
            thread::sleep(next.saturating_duration_since(Instant::now()));
        }
    });
    let trace_before = host.view().strip.trace.len();
    // Ana's own pings, not only the interval her view works out: the first
    // video wakes her timer thread, so they go every 100 ms at once and not
    // after the ping planned at the idle rate, up to a second away.
    ana.wait_for(
        Duration::from_millis(600),
        "three of Ana's own pings answered",
        |v| v.strip.trace.len() >= ana_before + 3,
    );
    println!(
        "Ana: three pings of her own answered {:.0} ms after the first frame",
        started.elapsed().as_secs_f64() * 1000.0
    );
    for (who, member) in [("Ana", ana), ("the host", &host)] {
        member.wait_for(Duration::from_millis(500), "pings at the media rate", |v| {
            v.numbers.ping_interval == Duration::from_millis(100)
        });
        println!(
            "{who}: pings every 100 ms {:.0} ms after the first frame",
            started.elapsed().as_secs_f64() * 1000.0
        );
    }
    let view = host.wait_for(Duration::from_secs(1), "the strip from video", |v| {
        v.strip.loss_from == Source::Video && v.strip.jitter_from == Source::Video
    });
    thread::sleep(Duration::from_millis(1000));
    let during = host.view();
    let points = during.strip.trace.len() - trace_before;
    sender.join().expect("the sender");
    println!(
        "while Ana's video flowed: the host's strip read loss {:?} % and jitter {:?} ms from it, and its trace grew by {points} points in about 1.2 s",
        view.strip.loss_pct, view.strip.jitter_ms
    );
    assert!(points >= 8, "{points} pings");
    assert_eq!(during.strip.loss_pct, Some(0.0));
    assert!(ana.view().numbers.video_sent > 0);
    let stopped = Instant::now();
    host.wait_for(Duration::from_secs(4), "idle again after the video", |v| {
        v.numbers.ping_interval == Duration::from_secs(1) && v.strip.loss_from == Source::Pings
    });
    println!(
        "pings back to once a second {:.1} s after the video stopped",
        stopped.elapsed().as_secs_f32()
    );
}

// 1.3.0 is the first copy that tells a watcher how the share stands and
// whether its PC sound goes out: the host in its answer to each Watch that
// starts a watch, then as either changes.
const V1_3_0: invite::Version = invite::Version {
    major: 1,
    minor: 3,
    patch: 0,
};

fn on_1_3_0(name: &str, timers: Timers) -> Config {
    let mut config = common::config(name, timers);
    config.version = V1_3_0;
    config
}

// Mara hosts, and Bo, a friend's program played by hand, shares, both on
// 1.3.0. The invite is multi-use, for Ana after him.
fn bo_shares() -> (Member, Hand, String, u32) {
    bo_shares_on(Member::host_with(on_1_3_0("Mara", timers())))
}

// As bo_shares, on a host already open.
fn bo_shares_on(host: Member) -> (Member, Hand, String, u32) {
    host.room().new_invite(true);
    let code = host
        .wait_for(WAIT, "a multi-use invite", |v| {
            v.invite
                .as_ref()
                .is_some_and(|i| i.multi_use && !i.code.is_empty())
        })
        .invite
        .expect("an invite")
        .code;
    let to_host = loopback(host.port());
    let mut bo = Hand::join_as(&code_to_invite(&code, to_host), to_host, "Bo", V1_3_0);
    // ShareStart, kind 9, at 60 fps.
    bo.say(&[9, 60]);
    let view = until(&host, &mut bo, "Bo sharing", |v| {
        v.share.current.as_ref().is_some_and(|c| c.name == "Bo")
    });
    let share = view.share.current.expect("a share").number;
    (host, bo, code, share)
}

// Ana live with Bo's share in her roster: she and Mara watch it, and the
// host's answer to her Watch says it is live and its sound off.
fn both_watch(host: &Member, ana: &Member, bo: &mut Hand, share: u32) {
    until(ana, bo, "Bo's share in Ana's roster", |v| {
        v.strip.state == LinkState::Live
            && v.share.current.as_ref().is_some_and(|c| c.number == share)
    });
    ana.room().watch(share, true);
    host.room().watch(share, true);
    until(ana, bo, "the answer to Ana's Watch", |v| {
        v.share.watching && words(v) == (ShareState::Live, Some(false))
    });
}

// How the share watched stands and whether its PC sound goes out, as the
// panel shows them.
fn words(view: &View) -> (ShareState, Option<bool>) {
    (view.share.watched_state, view.share.watched_sound)
}

// ShareState, kind 27, as a sharer says it.
fn share_state(share: u32, state: ShareState) -> Vec<u8> {
    let mut out = vec![27];
    out.extend_from_slice(&share.to_le_bytes());
    out.push(match state {
        ShareState::Live => 0,
        ShareState::Paused => 1,
        ShareState::Away => 2,
    });
    out
}

// ShareSound, kind 28: whether the share's PC sound goes out.
fn share_sound(share: u32, on: bool) -> Vec<u8> {
    let mut out = vec![28];
    out.extend_from_slice(&share.to_le_bytes());
    out.push(u8::from(on));
    out
}

// As Member::wait_for, with Bo's pings answered meanwhile: a hand keeps its
// place in the room only while it sends.
fn until(member: &Member, bo: &mut Hand, what: &str, ok: impl Fn(&View) -> bool) -> View {
    let deadline = Instant::now() + WAIT;
    loop {
        bo.answer_pings();
        let view = member.view();
        if ok(&view) {
            return view;
        }
        assert!(
            Instant::now() < deadline,
            "{what}: not within {WAIT:?}. Last view: {view:#?}"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

// As Member::holds_for, with Bo's pings answered meanwhile.
fn holds(member: &Member, bo: &mut Hand, span: Duration, what: &str, ok: impl Fn(&View) -> bool) {
    let end = Instant::now() + span;
    while Instant::now() < end {
        bo.answer_pings();
        let view = member.view();
        assert!(ok(&view), "{what}. View: {view:#?}");
        thread::sleep(Duration::from_millis(5));
    }
}

// Bo answers his pings and says nothing else until `at`.
fn wait_until(bo: &mut Hand, at: Instant) {
    while Instant::now() < at {
        bo.answer_pings();
        thread::sleep(Duration::from_millis(5));
    }
}

// Ana stops watching Bo's share and watches it again while the pause the
// host told her first watch is held up on its way to her, and by her second
// Watch the share is live again. The pause reaches her after that Watch,
// ahead of the host's answer to it, and only the answer counts.
#[test]
fn a_watch_again_takes_no_word_of_the_watch_before() {
    use ShareState::{Live, Paused};
    let (host, mut bo, code, share) = bo_shares();
    let link = Link::new(loopback(host.port()));
    let ana = Member::join_with(
        on_1_3_0("Ana", timers()),
        Arc::new(keys::Identity::generate()),
        code_to_invite(&code, link.addr),
    );
    both_watch(&host, &ana, &mut bo, share);
    // Toward Ana only, and for well under reconnecting_after, so her link
    // and its session stay as they are.
    let held = common::link::Shape {
        blocked: true,
        ..Default::default()
    };
    link.shape(held, Default::default());
    let paused = Instant::now();
    bo.say(&share_state(share, Paused));
    until(&host, &mut bo, "the pause at the host's viewer", |v| {
        v.share.watched_state == Paused
    });
    ana.room().watch(share, false);
    until(&host, &mut bo, "Ana's Stop at the host", |v| {
        v.share
            .current
            .as_ref()
            .is_some_and(|c| c.watchers == Some(1))
    });
    // Past the 250 ms the host leaves between two words it passes on, so
    // this one goes at once.
    wait_until(&mut bo, paused + Duration::from_millis(300));
    bo.say(&share_state(share, Live));
    until(&host, &mut bo, "the live at the host's viewer", |v| {
        v.share.watched_state == Live
    });
    ana.room().watch(share, true);
    until(&host, &mut bo, "Ana's second Watch at the host", |v| {
        v.share
            .current
            .as_ref()
            .is_some_and(|c| c.watchers == Some(2))
    });
    link.shape(Default::default(), Default::default());
    until(&ana, &mut bo, "the answer to Ana's second Watch", |v| {
        v.share.watched_sound == Some(false)
    });
    holds(
        &ana,
        &mut bo,
        Duration::from_millis(300),
        "Ana took the pause told her first watch",
        |v| v.share.watched_state == Live,
    );
}

// Ana's session runs out while Bo's share is paused with its sound on, and
// while her link is down he says it is live and the sound off: words that
// go with the old session. Her new session asks for the watch again, and
// the host's answer says how the share stands now.
#[test]
fn a_new_session_hears_that_the_share_is_live_again() {
    use ShareState::{Live, Paused};
    let (host, mut bo, code, share) = bo_shares();
    let forwarder = Forwarder::new(loopback(host.port()));
    // A session that cannot rekey runs out 600 ms after it began.
    let short = Timers {
        rekey_after: Duration::from_millis(300),
        reject_after: Duration::from_millis(600),
        ..timers()
    };
    let log = fresh_log("share", "new-session", "client");
    let mut config = on_1_3_0("Ana", short);
    config.log = Some(log.clone());
    let ana = Member::join_with(
        config,
        Arc::new(keys::Identity::generate()),
        code_to_invite(&code, forwarder.addr),
    );
    both_watch(&host, &ana, &mut bo, share);
    bo.say(&share_state(share, Paused));
    bo.say(&share_sound(share, true));
    until(
        &ana,
        &mut bo,
        "Ana told the share is paused and its sound on",
        |v| words(v) == (Paused, Some(true)),
    );
    // Watch pressed on a panel a step behind, while she watches already: it
    // is refused, and leaves the share as Bo said.
    ana.room().watch(share, true);
    assert_eq!(
        words(&ana.view()),
        (Paused, Some(true)),
        "the refused press changed Bo's words"
    );

    // Past reject_after, and short of lost_after.
    forwarder.block(true);
    let blocked = Instant::now();
    bo.say(&share_state(share, Live));
    bo.say(&share_sound(share, false));
    until(
        &host,
        &mut bo,
        "the live and the off at the host's viewer",
        |v| words(v) == (Live, Some(false)),
    );
    wait_until(&mut bo, blocked + Duration::from_millis(1500));
    forwarder.block(false);
    let live_and_off = |v: &View| words(v) == (Live, Some(false));
    until(
        &ana,
        &mut bo,
        "the answer to the Watch Ana's new session asked again",
        live_and_off,
    );
    holds(
        &ana,
        &mut bo,
        Duration::from_millis(300),
        "Ana's words changed after the answer",
        live_and_off,
    );
    drop(ana);
    let text = read_log(&log);
    assert!(
        text.contains("the session ran out without a rekey"),
        "{text}"
    );
}

// Mara's host restarts while Ana's link is down. Her session runs out short
// of lost_after, so she joins the new run as a known device and asks again,
// by number, for the share she watched. The new run's numbers start
// elsewhere: that Watch finds no share, and her first roster ends the watch.
#[test]
fn a_watch_asked_again_after_a_host_restart_finds_no_share() {
    let dir = Folder::new("share-restart");
    let identity = Arc::new(keys::Identity::generate());
    let mara = || {
        let mut config = config_in("Mara", timers(), dir.0.clone());
        config.version = V1_3_0;
        config
    };
    let (mut first, mut bo, code, share) =
        bo_shares_on(Member::host_as(Arc::clone(&identity), mara()));
    let port = first.port();
    let forwarder = Forwarder::new(loopback(port));
    // A session that cannot rekey runs out 600 ms after it began, long
    // before she would be lost.
    let short = Timers {
        rekey_after: Duration::from_millis(300),
        reject_after: Duration::from_millis(600),
        lost_after: Duration::from_secs(10),
        ..timers()
    };
    let ana_log = fresh_log("share", "restart", "client");
    let mut config = on_1_3_0("Ana", short);
    config.log = Some(ana_log.clone());
    let ana = Member::join_with(
        config,
        Arc::new(keys::Identity::generate()),
        code_to_invite(&code, forwarder.addr),
    );
    until(&ana, &mut bo, "Bo's share in Ana's roster", |v| {
        v.strip.state == LinkState::Live
            && v.share.current.as_ref().is_some_and(|c| c.number == share)
    });
    ana.room().watch(share, true);
    until(&ana, &mut bo, "the answer to Ana's Watch", |v| {
        v.share.watching && words(v) == (ShareState::Live, Some(false))
    });
    // The next run takes her back only from the list this one saved. Read
    // before the block, so the block stays short.
    let key = *ana.identity.public();
    poll(Duration::from_secs(5), "Ana on the host's list", || {
        bo.answer_pings();
        let devices = Room::known_devices(&dir.0).expect("the list reads");
        devices.devices.iter().any(|d| d.key == key).then_some(())
    });

    // Past reject_after, and short of lost_after.
    forwarder.block(true);
    let blocked = Instant::now();
    first.leave();
    let host_log = fresh_log("share", "restart", "host");
    let mut config = mara();
    config.port = port;
    config.log = Some(host_log.clone());
    let second = Member::host_as(identity, config);
    let hosts = shares(&second, 60);
    assert_ne!(
        hosts, share,
        "the new run's share has the number Ana watched"
    );
    thread::sleep((blocked + Duration::from_secs(1)).saturating_duration_since(Instant::now()));
    forwarder.block(false);
    let unblocked = Instant::now();
    let view = ana.wait_for(
        Duration::from_secs(10),
        "Ana back in the new run, watching nothing",
        |v| {
            v.strip.state == LinkState::Live
                && v.share.current.as_ref().is_some_and(|c| c.number == hosts)
                && !v.share.watching
        },
    );
    println!(
        "Ana watched share {share} of the first run; {:.0} ms after her link came back the second run's share {hosts} was in her roster and her watch had ended",
        unblocked.elapsed().as_secs_f64() * 1000.0
    );
    let lines = system_lines(&view);
    for line in ["Bo stopped sharing", "Mara started sharing"] {
        assert!(lines.iter().any(|l| l == line), "{lines:?}");
    }
    drop(ana);
    drop(second);
    let text = read_log(&ana_log);
    assert!(
        text.contains("the session ran out without a rekey"),
        "{text}"
    );
    assert!(
        !text.contains("lost, back to the invite addresses"),
        "{text}"
    );
    let text = read_log(&host_log);
    assert!(!text.contains("watches share"), "{text}");
}

// 1.3.0 is the first copy that reads a pointer update's mark, which says
// the update came with the picture about to be encoded. An older one refuses
// the whole update for it.
const MARK_SINCE: invite::Version = invite::Version {
    major: 1,
    minor: 3,
    patch: 0,
};
const OLDER: invite::Version = invite::Version {
    major: 1,
    minor: 2,
    patch: 0,
};

fn standing_for(name: &str, version: invite::Version) -> Config {
    let mut config = common::config(name, timers());
    config.version = version;
    config
}

// As room_of, with a config for each.
fn room_with(host: Config, friends: Vec<Config>) -> (Member, Vec<Member>, Invite) {
    let host = Member::host_with(host);
    host.room().new_invite(true);
    let view = host.wait_for(WAIT, "a multi-use invite", |v| {
        v.invite
            .as_ref()
            .is_some_and(|i| i.multi_use && !i.code.is_empty())
    });
    let invite = code_to_invite(&view.invite.expect("an invite").code, loopback(host.port()));
    let people = friends.len() + 1;
    let friends: Vec<Member> = friends
        .into_iter()
        .map(|config| {
            Member::join_with(config, Arc::new(keys::Identity::generate()), invite.clone())
        })
        .collect();
    for friend in &friends {
        friend.wait_for(WAIT, "live with everyone", |v| {
            v.strip.state == LinkState::Live && v.people.len() == people
        });
    }
    host.wait_for(WAIT, "everyone in", |v| v.people.len() == people);
    (host, friends, invite)
}

fn watched_by(member: &Member, share: u32, watchers: u8) {
    member.wait_for(WAIT, "the watchers counted", |v| {
        v.share
            .current
            .as_ref()
            .is_some_and(|c| c.number == share && c.watchers == Some(watchers))
    });
}

// Whether the pointer update at `x` that reached `member` for `share` was
// marked, or None when it did not come within a second.
fn marked_at(member: &Member, share: u32, x: i32) -> Option<bool> {
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut batch = Batch::default();
    loop {
        member.room().watching().take(&mut batch);
        if let Some((at, pointer)) = batch.pointer
            && at == share
            && pointer.x == x
        {
            return Some(pointer.with_picture);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

// The flags of the pointer update at `x` as they reached a hand, read from
// the packet: kind 0, slot 1, seq 2, x 6, y 10, flags 14, shape 15. None
// when it did not come within 200 ms.
fn flags_at(hand: &mut Hand, x: i32) -> Option<u8> {
    let deadline = Instant::now() + Duration::from_millis(200);
    loop {
        hand.answer_pings();
        let came = hand.received().into_iter().find(|(channel, payload)| {
            *channel == Channel::Cursor && payload.len() == 19 && payload[6..10] == x.to_le_bytes()
        });
        if let Some((_, payload)) = came {
            return Some(payload[14]);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

// The control message Hello: kind 24, the protocol and the version, each
// little-endian, the name, and no address.
fn hello(name: &str, version: invite::Version) -> Vec<u8> {
    let mut hello = vec![24];
    for part in [
        invite::PROTOCOL,
        version.major,
        version.minor,
        version.patch,
    ] {
        hello.extend_from_slice(&part.to_le_bytes());
    }
    hello.push(name.len() as u8);
    hello.extend_from_slice(name.as_bytes());
    hello.push(0);
    hello
}

// Marked updates from `outbox`, the next x each time, until one reaches the
// hand with `flags`: the x it came at, or None when none did within WAIT.
fn marked_until(
    outbox: &mut room::screen::Outbox,
    hand: &mut Hand,
    x: &mut i32,
    flags: u8,
) -> Option<i32> {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        *x += 1;
        outbox.pointer(*x, 40, true, 0, true);
        if flags_at(hand, *x) == Some(flags) {
            return Some(*x);
        }
    }
    None
}

// A pointer update's mark reaches only copies that read it. The host clears
// it for a watcher standing for 1.2.0; a sharer marks only toward a host
// whose copy reads it, and only from a room standing for 1.3.0; a host that
// shares marks for each watcher by that watcher's latest Hello, a Hello that
// changes on the same session and address included, and only from a room
// standing for 1.3.0. Nobody counts any of it bad, and a host standing for
// 1.2.0 refuses a marked update that comes anyway, as such a copy does.
#[test]
fn a_pointer_mark_reaches_only_copies_that_read_it() {
    let (host, friends, invite) = room_with(
        standing_for("Mara", MARK_SINCE),
        vec![
            standing_for("Ana", MARK_SINCE),
            standing_for("Bo", MARK_SINCE),
            standing_for("Cy", OLDER),
        ],
    );
    let (ana, bo, cy) = (&friends[0], &friends[1], &friends[2]);
    let bad = |member: &Member| member.view().numbers.dropped_bad;
    let (mara_bad, cy_bad) = (bad(&host), bad(cy));

    // Ana's share, passed on by Mara to Bo and Cy, and watched by Mara.
    let share = shares(ana, 60);
    for member in [bo, cy, &host] {
        watches(member, share);
    }
    watched_by(&host, share, 3);
    let mut outbox = ana.room().sharing().outbox();
    let marks = |x: i32| [&host, bo, cy].map(|member| marked_at(member, share, x));
    outbox.pointer(10, 20, true, 0, true);
    assert_eq!(marks(10), [Some(true), Some(true), Some(false)]);
    outbox.pointer(11, 20, true, 0, false);
    assert_eq!(marks(11), [Some(false); 3]);

    // Mara's own share to Bo, Cy and Eve, a hand whose Hello says 1.2.0,
    // then 1.3.0, then 1.2.0 again, on one session and address.
    ana.room().stop_sharing();
    host.wait_for(WAIT, "Ana's share over", |v| v.share.current.is_none());
    let share = shares(&host, 60);
    for member in [bo, cy] {
        watches(member, share);
    }
    let mut eve = Hand::join_as(&invite, loopback(host.port()), "Eve", OLDER);
    let mut watch = vec![12];
    watch.extend_from_slice(&share.to_le_bytes());
    watch.extend_from_slice(&[1, 1]);
    eve.say(&watch);
    until(&host, &mut eve, "Eve watching too", |v| {
        v.share
            .current
            .as_ref()
            .is_some_and(|c| c.watchers == Some(3))
    });
    let mut outbox = host.room().sharing().outbox();
    outbox.pointer(30, 40, true, 0, true);
    assert_eq!(
        [bo, cy].map(|member| marked_at(member, share, 30)),
        [Some(true), Some(false)]
    );
    assert_eq!(flags_at(&mut eve, 30), Some(0x01), "Eve standing for 1.2.0");
    let mut x = 30;
    eve.say(&hello("Eve", MARK_SINCE));
    let read = marked_until(&mut outbox, &mut eve, &mut x, 0x03);
    assert!(
        read.is_some(),
        "no mark reached Eve after her Hello said 1.3.0"
    );
    eve.say(&hello("Eve", OLDER));
    let cleared = marked_until(&mut outbox, &mut eve, &mut x, 0x01);
    assert!(
        cleared.is_some(),
        "the mark still reached Eve after her Hello said 1.2.0"
    );

    // Cy's share, from a room standing for 1.2.0, to Mara and Bo: unmarked
    // all the way.
    host.room().stop_sharing();
    cy.wait_for(WAIT, "Mara's share over", |v| v.share.current.is_none());
    let share = shares(cy, 60);
    for member in [bo, &host] {
        watches(member, share);
    }
    watched_by(&host, share, 2);
    let mut outbox = cy.room().sharing().outbox();
    outbox.pointer(50, 60, true, 0, true);
    assert_eq!(
        [&host, bo].map(|member| marked_at(member, share, 50)),
        [Some(false); 2]
    );
    assert_eq!((bad(&host), bad(cy)), (mara_bad, cy_bad));
    println!(
        "Eve's updates came marked from x {read:?} after her Hello said 1.3.0, and unmarked from x {cleared:?} after it said 1.2.0"
    );
    drop(eve);
    drop(friends);
    drop(host);

    // A host standing for 1.2.0: Ana's marked update reaches Bo unmarked,
    // the host's own share marks nothing, and a marked update that comes
    // anyway, from a hand, is refused and counted bad, as such a copy does.
    let (host, friends, invite) = room_with(
        standing_for("Mara", OLDER),
        vec![
            standing_for("Ana", MARK_SINCE),
            standing_for("Bo", MARK_SINCE),
        ],
    );
    let (ana, bo) = (&friends[0], &friends[1]);
    let before = bad(&host);
    let share = shares(ana, 60);
    watches(bo, share);
    watched_by(&host, share, 1);
    ana.room().sharing().outbox().pointer(70, 80, true, 0, true);
    assert_eq!(marked_at(bo, share, 70), Some(false));
    ana.room().stop_sharing();
    host.wait_for(WAIT, "Ana's share over", |v| v.share.current.is_none());
    let share = shares(&host, 60);
    watches(bo, share);
    watched_by(&host, share, 1);
    host.room()
        .sharing()
        .outbox()
        .pointer(90, 80, true, 0, true);
    assert_eq!(marked_at(bo, share, 90), Some(false));
    assert_eq!(bad(&host), before);
    host.room().stop_sharing();
    host.wait_for(WAIT, "Mara's share over", |v| v.share.current.is_none());
    let mut dee = Hand::join_as(&invite, loopback(host.port()), "Dee", MARK_SINCE);
    // ShareStart, kind 9, at 60 fps.
    dee.say(&[9, 60]);
    until(&host, &mut dee, "Dee sharing", |v| {
        v.share.current.as_ref().is_some_and(|c| c.name == "Dee")
    });
    // Kind 1 (sent to the host) and slot 0, seq 0, x and y, VISIBLE and
    // WITH_PICTURE, shape 0.
    let mut marked = vec![1, 0];
    marked.extend_from_slice(&0u32.to_le_bytes());
    marked.extend_from_slice(&110i32.to_le_bytes());
    marked.extend_from_slice(&80i32.to_le_bytes());
    marked.push(0x03);
    marked.extend_from_slice(&0u32.to_le_bytes());
    dee.send(Channel::Cursor, &marked);
    until(&host, &mut dee, "the marked update counted bad", |v| {
        v.numbers.dropped_bad > before
    });
}
