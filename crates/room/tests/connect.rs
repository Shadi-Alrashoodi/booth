mod common;

use std::time::{Duration, Instant};

use common::{Member, host_invite, timers};
use room::Timers;
use room::view::{LinkState, Notice, Role, TracePoint};

#[test]
fn join_connects_and_measures() {
    let host = Member::host("Host", timers());
    let invite = host_invite(&host);
    let started = Instant::now();
    let client = Member::join("Ana", timers(), invite);

    client.wait_for(Duration::from_secs(1), "client live", |v| {
        v.strip.state == LinkState::Live
    });
    host.wait_for(
        Duration::from_secs(1).saturating_sub(started.elapsed()),
        "host live",
        |v| v.strip.state == LinkState::Live,
    );

    let seen = client.wait_for(Duration::from_secs(3), "round trip, jitter and loss", |v| {
        v.strip.rtt_ms.is_some() && v.strip.jitter_ms.is_some() && v.strip.loss_pct.is_some()
    });
    assert_eq!(seen.role, Role::Client);
    assert_eq!(seen.room_name, "Host's room");
    assert_eq!(seen.numbers.link_name.as_deref(), Some("Host"));
    assert_eq!(seen.strip.loss_pct, Some(0.0));

    let hosted = host.wait_for(Duration::from_secs(2), "both names in the roster", |v| {
        v.people.len() == 2 && v.people[1].name == "Ana"
    });
    assert_eq!(hosted.people[0].name, "Host");
    assert!(hosted.people[0].is_host && hosted.people[0].is_you);
    assert!(hosted.people[1].joined_by_invite && !hosted.people[1].is_you);
    assert_eq!(hosted.people[1].key, *client.identity.public());
    assert_eq!(hosted.numbers.link_name.as_deref(), Some("Ana"));

    let seen = client.wait_for(Duration::from_secs(2), "the client's own roster", |v| {
        v.people.len() == 2 && v.people.iter().any(|p| p.is_you && p.name == "Ana")
    });
    assert!(seen.people.iter().any(|p| p.is_host && p.name == "Host"));
    let host_row = seen.people.iter().find(|p| p.is_host).unwrap();
    assert_eq!(host_row.fingerprint, host.identity.fingerprint());

    let numbers = seen.numbers;
    let connect_ms = numbers.connect_ms.expect("connect_ms is filled");
    let handshake_ms = numbers.handshake_ms.expect("handshake_ms is filled");
    assert!(connect_ms < 1000.0, "connect took {connect_ms} ms");
    assert!(handshake_ms <= connect_ms);
    // Both ends run on this PC, so their clocks are the same clock.
    let offset_ms = numbers.clock_offset_ms.expect("clock offset is filled");
    assert!(
        offset_ms.abs() < 20.0,
        "clock offset {offset_ms} ms on one PC"
    );
    assert!(numbers.session_age.is_some());
    assert!(numbers.peer_addr.is_some());
    assert_eq!(numbers.ping_interval, Duration::from_millis(100));
    println!(
        "connect_ms {connect_ms:.2}, handshake_ms {handshake_ms:.2}, rtt_ms {:.3}, jitter_ms {:.3}",
        numbers.rtt_ms.unwrap_or(f32::NAN),
        numbers.jitter_ms.unwrap_or(f32::NAN),
    );
}

#[test]
fn rekeys_keep_the_link_live() {
    let short = Timers {
        rekey_after: Duration::from_millis(300),
        reject_after: Duration::from_millis(600),
        ..timers()
    };
    let host = Member::host("Host", short);
    let client = Member::join("Ana", short, host_invite(&host));
    client.wait_for(Duration::from_secs(1), "client live", |v| {
        v.strip.state == LinkState::Live
    });
    host.wait_for(Duration::from_secs(1), "host live", |v| {
        v.strip.state == LinkState::Live
    });

    // Three seconds of rekeys, then as long again as a ping takes to count
    // as lost, so the pings from the last of them are judged too.
    let rekeying = Instant::now();
    let end = rekeying + Duration::from_secs(3) + stats::LOST_AFTER + Duration::from_millis(300);
    while Instant::now() < end {
        client.holds_for(Duration::from_millis(50), "client stays live", |v| {
            v.strip.state == LinkState::Live
        });
        host.holds_for(Duration::from_millis(50), "host stays live", |v| {
            v.strip.state == LinkState::Live
        });
    }

    let seen = client.view();
    let hosted = host.view();
    println!(
        "rekeys: client {}, host {}; pings judged on the client: {}",
        seen.numbers.rekeys,
        hosted.numbers.rekeys,
        seen.strip.trace.len()
    );
    assert!(seen.numbers.rekeys >= 4, "{:#?}", seen.numbers);
    assert!(hosted.numbers.rekeys >= 4, "{:#?}", hosted.numbers);
    // Pings kept going out through every rekey: over 3 s at 100 ms, less
    // what late timer wakeups cost.
    assert!(
        seen.strip.trace.len() >= 20,
        "only {} pings judged",
        seen.strip.trace.len()
    );
    for (who, view) in [("client", &seen), ("host", &hosted)] {
        assert!(
            !view.strip.trace.contains(&TracePoint::Lost),
            "{who} lost a ping: {:?}",
            view.strip.trace
        );
        assert_eq!(view.strip.loss_pct, Some(0.0), "{who}");
        assert_eq!(view.numbers.dropped_bad, 0, "{who}");
    }
}

#[test]
fn client_leaving_leaves_the_host_alone() {
    let host = Member::host("Host", timers());
    let mut client = Member::join("Ana", timers(), host_invite(&host));
    host.wait_for(Duration::from_secs(1), "host live", |v| {
        v.strip.state == LinkState::Live && v.people.len() == 2
    });

    let took = client.leave();
    assert!(took < Duration::from_millis(200), "leave took {took:?}");
    let alone = host.wait_for(Duration::from_secs(1), "host alone again", |v| {
        v.strip.state == LinkState::Alone
    });
    assert_eq!(alone.people.len(), 1);
    assert!(alone.strip.rtt_ms.is_none() && alone.numbers.link_name.is_none());
}

#[test]
fn host_leaving_closes_the_room() {
    let mut host = Member::host("Host", timers());
    let client = Member::join("Ana", timers(), host_invite(&host));
    client.wait_for(Duration::from_secs(1), "client live", |v| {
        v.strip.state == LinkState::Live
    });
    host.wait_for(Duration::from_secs(1), "host live", |v| {
        v.strip.state == LinkState::Live
    });

    let took = host.leave();
    assert!(took < Duration::from_millis(200), "leave took {took:?}");
    let closed = client.wait_for(Duration::from_secs(1), "room closed", |v| {
        v.notice == Some(Notice::RoomClosed)
    });
    assert_eq!(closed.strip.state, LinkState::Closed);
}
