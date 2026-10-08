// Answers from somewhere other than the host's address. Someone who sees the
// host's packets can send a copy first from an address of their own, and the
// host holds the session keys, so it can seal any answer and send it from
// anywhere it can spoof.

use super::*;

// A copy of the host's answer, sent first from an address the host does not
// have, is refused and moves nothing there. The real answer that follows
// from the host's address is still taken, and the join goes on there.
#[test]
fn answer_from_elsewhere_is_refused() {
    let start = Instant::now();
    let mut rig = Rig::new(Timers::default(), start);
    let home = rig.host.wire.addr();
    rig.tick(start);
    let (_, mut session, response) = rig.host.answer_all(start).pop().expect("a try");

    let copier = Wire::at(Ipv4Addr::new(127, 0, 0, 2));
    let bad = rig.client.drops.bad;
    rig.client
        .on_packet(&response, copier.addr(), start, &rig.socket);
    assert_eq!(rig.client.host_addr, None, "the copy moved the host");
    assert!(rig.client.media.is_none(), "the copy moved the voice");
    assert_eq!(rig.client.drops.bad, bad + 1);
    assert_eq!(rig.client.attempts.len(), 1, "the try is still open");

    rig.deliver(&response, start);
    let said = told_host(&mut rig.host, &mut session);
    assert!(matches!(said[..], [Message::Hello { .. }]), "{said:?}");
    let hello = rig.host.hello(&mut session, start);
    rig.deliver(&hello, start);
    let secret = rig.host.send_secret(&mut session, start);
    rig.deliver(&secret, start);
    rig.deliver(&ping(&mut session, 0), start);
    assert_eq!(rig.client.state, LinkState::Live);
    assert_eq!(rig.client.host_addr, Some(home));
    assert_eq!(rig.client.media.map(|media| media.to()), Some(home));
    assert!(copier.packets().is_empty(), "the copier got packets");
}

// A host that wants this PC's voice sent at someone else spoofs one of its
// packets from there, which moves the pings and control, then a pong from
// there for the ping that went there. It never sees that ping, so it makes
// the pong up, for each seq the count gives and with a send time later than
// any ping. None of them moves the voice.
#[test]
fn made_up_pong_keeps_uplink() {
    let start = Instant::now();
    let (mut rig, mut session) = Rig::connected(Timers::default(), start);
    let home = rig.host.wire.addr();
    let victim = Wire::at(Ipv4Addr::new(127, 0, 0, 3));

    let spoofed = ping(&mut session, 1);
    rig.client
        .on_packet(&spoofed, victim.addr(), start, &rig.socket);
    assert_eq!(rig.client.host_addr, Some(victim.addr()));
    rig.tick(start);
    assert!(
        !victim.packets().is_empty(),
        "no ping went to the new address"
    );

    for seq in 0..8 {
        let mut pong = Vec::new();
        PingMessage::Pong {
            seq,
            t1: u64::MAX,
            t2: 0,
            t3: 0,
        }
        .encode(&mut pong);
        let pong = seal(&mut session, Channel::Ping, &pong);
        rig.client
            .on_packet(&pong, victim.addr(), start, &rig.socket);
    }
    assert_eq!(rig.client.media.map(|media| media.to()), Some(home));
}
