// Where the host sends a friend's media, what may move it, and how much of a
// friend's share it passes on.

use super::*;
use voice::codec::{Encoder, Mode};

// A far side at an address of its own, where nobody in the room is.
fn elsewhere(n: u8) -> Wire {
    Wire::at(Ipv4Addr::new(127, 0, 7, n))
}

// Where control and pings go for `guest`, and where media goes.
fn sent_to(rig: &Rig, guest: &Guest) -> (SocketAddr, SocketAddr) {
    let key = *guest.identity.public();
    let peer = rig
        .host
        .peers
        .iter()
        .find(|peer| peer.key == key)
        .expect("in the room");
    (peer.addr, peer.media.to())
}

// One 5 ms voice frame from `guest`, sealed for the host.
fn spoken(guest: &mut Guest, seq: u16) -> Vec<u8> {
    let mut encoder = Encoder::new(Mode::LowDelay, true).unwrap();
    let pcm: Vec<f32> = (0..240).map(|n| 0.2 * (n as f32 * 0.06).sin()).collect();
    let mut opus = [0u8; 20];
    let len = encoder.encode(&pcm, &mut opus).unwrap();
    let frame = talk::Frame {
        seq,
        captured: 0,
        mode: Mode::LowDelay,
        redundancy: false,
        last: false,
        frame: &opus[..len],
        previous: None,
        pad: 0,
    };
    let mut payload = Vec::new();
    frame.write_spoken(&mut payload);
    seal(
        guest.session.as_mut().expect("joined"),
        Channel::Voice,
        &payload,
    )
}

// Ana holds her own session keys, so she can seal a pong for any ping, with
// a send time later than every ping. A packet of hers from someone else's
// address moves control there, and the host pings that address to check
// it. Her made-up pong from there passes the link's checks, but must not
// take the voice relayed to her there too.
#[test]
fn made_up_pong_keeps_media() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let mut bo = Guest::new();
    bo.join(&mut rig, "Bo", start);
    // The host's first pings go out.
    rig.tick(start);
    let home = ana.wire.addr();
    let victim = elsewhere(1);

    let packet = ping(ana.session.as_mut().unwrap(), 0);
    rig.deliver(&packet, victim.addr(), start);
    rig.tick(start);

    // From there: a pong for the host's first ping, which went home, sent
    // later than any ping could be.
    let bad = rig.host.drops.bad;
    let mut pong = Vec::new();
    PingMessage::Pong {
        seq: 0,
        t1: u64::MAX,
        t2: 0,
        t3: 0,
    }
    .encode(&mut pong);
    let pong = seal(ana.session.as_mut().unwrap(), Channel::Ping, &pong);
    rig.deliver(&pong, victim.addr(), start);
    assert_eq!(rig.host.drops.bad, bad, "the link took the pong");
    assert_eq!(sent_to(&rig, &ana), (victim.addr(), home));

    let said = spoken(&mut bo, 0);
    rig.deliver(&said, bo.wire.addr(), start);
    let at_victim = opened(ana.session.as_mut().unwrap(), victim.packets());
    assert_eq!(voice_count(&at_victim), 0, "voice went to the victim");
    let at_home = opened(ana.session.as_mut().unwrap(), ana.wire.packets());
    assert_eq!(voice_count(&at_home), 1);
}

// The host answers a newcomer's initiation where it came from, the one
// address the handshake made a round trip to. The data packet that confirms
// it can come from anywhere: control goes there, and media stays where the
// answer went until a ping to the new address is answered from there.
#[test]
fn confirm_from_elsewhere_keeps_media() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    let initiation = ana.knock(&mut rig, start);
    ana.session = Some(ana.answer(initiation, start).expect("the host answers"));
    let answered = ana.wire.addr();
    let there = elsewhere(2);
    let confirm = ping(ana.session.as_mut().unwrap(), 0);
    rig.deliver(&confirm, there.addr(), start);
    assert_eq!(rig.host.peers.len(), 1);
    assert_eq!(sent_to(&rig, &ana), (there.addr(), answered));

    // A real move: the ping to the new address comes back from there.
    rig.tick(start);
    let (seq, t1) = opened(ana.session.as_mut().unwrap(), there.packets())
        .iter()
        .find_map(|plain| match peer::read_plain(plain) {
            Some(Plain::Ping(PingMessage::Ping { seq, t1 })) => Some((seq, t1)),
            _ => None,
        })
        .expect("a ping to the new address");
    let mut pong = Vec::new();
    PingMessage::Pong {
        seq,
        t1,
        t2: t1,
        t3: t1,
    }
    .encode(&mut pong);
    let pong = seal(ana.session.as_mut().unwrap(), Channel::Ping, &pong);
    // The ping was stamped with the real time it was built, so its answer
    // comes in at a real time after that.
    rig.deliver(&pong, there.addr(), Instant::now());
    assert_eq!(sent_to(&rig, &ana), (there.addr(), there.addr()));
}

// The cap in the facts is only advice to the sharer. A friend who ignores
// it sends its whole intake burst of full 1200-byte datagrams at once, to
// three watchers over the internet. The host passes on no more than twice
// its upload setting a second, a second of it at once; the rest is dropped,
// and none of it counts as bad.
#[test]
fn relayed_share_held_to_upload() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut guests: Vec<Guest> = ["Ana", "Bo", "Cy", "Dee"]
        .iter()
        .map(|name| {
            let mut guest = Guest::new();
            guest.join(&mut rig, name, start);
            guest
        })
        .collect();
    for watcher in &guests[1..] {
        let at = peer_at(&mut rig, watcher);
        rig.host.peers[at].path = PathWord::Direct;
    }
    let share = granted(&mut rig, &mut guests[0], start);
    for watcher in &mut guests[1..] {
        let watch = Message::Watch {
            share,
            on: true,
            hevc: true,
        };
        watcher.say(&mut rig, &watch, start);
    }

    let payload =
        crate::screen::INTERNET_DATAGRAM - session::DATA_OVERHEAD - 1 - crate::screen::wire::PREFIX;
    let facts = channels::video::FrameFacts::default();
    let mut packetizer = channels::video::Packetizer::new(payload).expect("a packetizer");
    let packets = packetizer
        .packetize(&facts, &vec![7; 64 * 1024], 20)
        .expect("packetized");
    let full = packets.iter().next().expect("a packet").to_vec();
    let copy = (1 + crate::screen::wire::PREFIX + full.len() + session::DATA_OVERHEAD) as u64;
    assert!(copy > (crate::screen::INTERNET_DATAGRAM - channels::video::SHARD_STEP) as u64);
    let from = guests[0].wire.addr();
    let burst = crate::screen::VIDEO_BURST as u64;
    for _ in 0..burst {
        let packet = sent_video(&mut guests[0], &full);
        rig.deliver(&packet, from, start);
    }

    let budget = 2 * u64::from(rig.host.upload_kbps) * 1000 / 8;
    let passed_on = rig.host.screen.relayed * copy;
    println!(
        "{burst} packets of {copy} bytes at once to 3 watchers over the internet: {passed_on} bytes passed on, {budget} allowed"
    );
    assert!(passed_on <= budget, "{passed_on} bytes passed on");
    assert!(passed_on + copy > budget, "a second of it goes at once");
    let screen = &rig.host.screen;
    assert_eq!(screen.relayed + screen.dropped, burst * 3);
    assert_eq!(rig.host.drops.bad, 0);
}
