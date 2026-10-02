mod common;

use std::net::{Ipv6Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use common::{Forwarder, Member, code_to_invite, host_invite, invite_to, loopback, timers};
use invite::{Candidate, CandidateKind};
use room::Timers;
use room::view::{LinkState, TracePoint, View};

fn live(v: &View) -> bool {
    v.strip.state == LinkState::Live
}

#[test]
fn replayed_initiation_gets_no_answer() {
    let host = Member::host("Host", timers());
    let forwarder = Forwarder::new(loopback(host.port()));
    let client = Member::join("Ana", timers(), invite_to(&host, forwarder.addr));
    client.wait_for(Duration::from_secs(1), "client live", live);
    host.wait_for(Duration::from_secs(1), "host live", live);

    let captured = forwarder.initiations();
    let first = captured.first().expect("the forwarder saw an initiation");
    let replayer = UdpSocket::bind(loopback(0)).unwrap();
    replayer
        .set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    let mut buf = [0u8; 2048];

    // Right after the join a copy looks like the same try coming in over a
    // second path. It gets no answer either way.
    replayer.send_to(first, loopback(host.port())).unwrap();
    let answer = replayer.recv_from(&mut buf);
    assert!(answer.is_err(), "the host answered a copy: {answer:?}");

    // A second later it can only be a replay, and it is counted as one.
    client.holds_for(Duration::from_millis(800), "client live", live);
    let replays_before = host.view().numbers.dropped_replay;
    replayer.send_to(first, loopback(host.port())).unwrap();
    let answer = replayer.recv_from(&mut buf);
    assert!(answer.is_err(), "the host answered a replay: {answer:?}");
    let hosted = host.wait_for(Duration::from_secs(1), "replay counted", |v| {
        v.numbers.dropped_replay > replays_before
    });
    assert_eq!(hosted.people.len(), 2);

    let received = client.view().numbers.packets_received;
    client.wait_for(Duration::from_secs(1), "pings still flowing", |v| {
        v.numbers.packets_received >= received + 5
    });
    client.holds_for(Duration::from_millis(200), "client still live", live);
    let seen = client.view();
    assert_eq!(seen.strip.loss_pct, Some(0.0));
    assert_eq!(seen.numbers.rekeys, 0);
}

#[test]
fn try_over_two_paths_is_not_a_replay() {
    let host = Member::host("Host", timers());
    let mut invite = host_invite(&host);
    invite.candidates.push(Candidate {
        kind: CandidateKind::Lan,
        addr: SocketAddr::from((Ipv6Addr::LOCALHOST, host.port())),
    });
    let client = Member::join("Ana", timers(), invite);
    client.wait_for(Duration::from_secs(1), "client live", live);
    host.wait_for(Duration::from_secs(1), "host live", live);
    host.holds_for(
        Duration::from_millis(300),
        "a clean join shows no drops",
        |v| v.numbers.dropped_replay == 0 && v.numbers.dropped_bad == 0,
    );
}

#[test]
fn host_follows_a_client_that_moved() {
    let host = Member::host("Host", timers());
    let forwarder = Forwarder::new(loopback(host.port()));
    let client = Member::join("Ana", timers(), invite_to(&host, forwarder.addr));
    client.wait_for(Duration::from_secs(1), "client live", live);
    let before = forwarder.host_side();
    host.wait_for(Duration::from_secs(1), "host sees the first address", |v| {
        v.numbers.peer_addr == Some(before)
    });

    let moved = forwarder.rebind();
    assert_ne!(moved, before);
    host.wait_for(Duration::from_secs(1), "host follows", |v| {
        v.numbers.peer_addr == Some(moved)
    });

    let seen = client.view().numbers.packets_received;
    let hosted = host.view().numbers.packets_received;
    client.wait_for(Duration::from_secs(1), "pings reach the client", |v| {
        v.numbers.packets_received >= seen + 5
    });
    host.wait_for(Duration::from_secs(1), "pings reach the host", |v| {
        v.numbers.packets_received >= hosted + 5
    });
    client.holds_for(Duration::from_millis(200), "client still live", live);
    host.holds_for(Duration::from_millis(200), "host still live", live);
    assert_eq!(host.view().people.len(), 2);
}

#[test]
fn control_resumes_when_the_path_is_back() {
    // Nobody counts as reconnecting during the two-second blip.
    let patient = Timers {
        reconnecting_after: Duration::from_secs(3),
        lost_after: Duration::from_secs(6),
        ..timers()
    };
    let host = Member::host("Host", patient);
    host_invite(&host);
    host.room().new_invite(true);
    let view = host.wait_for(Duration::from_secs(1), "multi-use invite", |v| {
        v.invite
            .as_ref()
            .is_some_and(|i| i.multi_use && !i.code.is_empty())
    });
    let code = view.invite.unwrap().code;
    let forwarder = Forwarder::new(loopback(host.port()));
    let ana = Member::join("Ana", patient, code_to_invite(&code, forwarder.addr));
    ana.wait_for(Duration::from_secs(1), "Ana in a room of two", |v| {
        live(v) && v.people.len() == 2
    });
    // A measured round trip gives the host's retransmits their 20 ms floor
    // instead of the 200 ms they start from.
    host.wait_for(Duration::from_secs(1), "a round trip to Ana", |v| {
        v.strip.rtt_ms.is_some()
    });

    // Bo joins while nothing gets through to Ana, so the roster that names
    // him backs off to one retransmit every 1.3 s and then 2.5 s.
    forwarder.block(true);
    let bo = Member::join("Bo", patient, code_to_invite(&code, loopback(host.port())));
    bo.wait_for(Duration::from_secs(1), "Bo live", live);
    ana.holds_for(Duration::from_millis(1800), "Ana still live", live);
    forwarder.block(false);

    let back = Instant::now();
    ana.wait_for(
        Duration::from_millis(400),
        "the roster reaches Ana soon after the path is back",
        |v| v.people.len() == 3,
    );
    println!(
        "roster arrived {:?} after the path came back",
        back.elapsed()
    );
}

#[test]
fn reconnecting_keeps_the_last_round_trip() {
    // As with the real timers, a link counts as reconnecting only after the
    // stats have marked its newest ping lost.
    let slow = Timers {
        reconnecting_after: Duration::from_millis(2500),
        lost_after: Duration::from_secs(8),
        ..timers()
    };
    assert!(slow.reconnecting_after > stats::LOST_AFTER);
    let host = Member::host("Host", slow);
    let forwarder = Forwarder::new(loopback(host.port()));
    let client = Member::join("Ana", slow, invite_to(&host, forwarder.addr));
    client.wait_for(Duration::from_secs(1), "a round trip on the client", |v| {
        live(v) && v.strip.rtt_ms.is_some()
    });
    host.wait_for(Duration::from_secs(1), "a round trip on the host", |v| {
        live(v) && v.strip.rtt_ms.is_some()
    });

    forwarder.block(true);
    let seen = client.wait_for(Duration::from_secs(4), "client reconnecting", |v| {
        v.strip.state == LinkState::Reconnecting
    });
    assert!(seen.strip.trace.contains(&TracePoint::Lost));
    assert!(seen.strip.rtt_ms.is_some(), "{:?}", seen.strip);
    assert!(seen.numbers.rtt_ms.is_some());

    let hosted = host.wait_for(Duration::from_secs(1), "host reconnecting", |v| {
        v.strip.state == LinkState::Reconnecting
    });
    assert!(hosted.strip.rtt_ms.is_some(), "{:?}", hosted.strip);
    assert!(hosted.people[1].reconnecting && hosted.people[1].rtt_ms.is_some());
}

#[test]
fn oversized_datagrams_count_as_dropped() {
    let host = Member::host("Host", timers());
    host_invite(&host);
    let junk = UdpSocket::bind(loopback(0)).unwrap();
    junk.send_to(&[0x99; 4000], loopback(host.port())).unwrap();
    junk.send_to(&[0x99], loopback(host.port())).unwrap();
    host.wait_for(Duration::from_secs(1), "both junk packets counted", |v| {
        v.numbers.dropped_bad >= 2
    });
}
