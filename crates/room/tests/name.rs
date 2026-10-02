mod common;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{
    FakeNameserver, FakeStun, FakeSystem, Member, NAME, OUTSIDE, code_to_invite, fresh_log, lookup,
    read_log, timers,
};
use invite::Invite;
use keys::Identity;
use room::view::{LinkState, NameAnswer, NameMatch, View};
use room::{Lookup, Timers};

// Windows has no route to 0.0.0.0/8, so a try sent there never leaves this
// PC, and the port in it is still the one the client pairs the name with.
const NOWHERE: Ipv4Addr = Ipv4Addr::new(0, 0, 0, 9);

fn quick() -> Timers {
    Timers {
        still_trying_after: Duration::from_millis(500),
        ..timers()
    }
}

fn found(view: &View) -> Option<NameAnswer> {
    view.numbers
        .address_name
        .as_ref()
        .map(|name| name.answer.clone())
        .filter(|answer| !matches!(answer, NameAnswer::NotAsked | NameAnswer::Looking))
}

// Live, and the first pong is back, which is when the log says so.
fn connected(v: &View) -> bool {
    v.strip.state == LinkState::Live && v.numbers.connect_ms.is_some()
}

// A host that answers on loopback and puts NAME in its invite.
fn host_with_name(lookup: Lookup, log: PathBuf) -> (Member, Invite) {
    let mut config = common::config("Host", timers());
    config.address_name = Some(NAME.to_owned());
    config.lookup = lookup;
    config.log = Some(log);
    let host = Member::host_with(config);
    let view = host.wait_for(Duration::from_secs(2), "the host's own lookup", |v| {
        found(v).is_some() && v.invite.as_ref().is_some_and(|i| !i.code.is_empty())
    });
    let code = view.invite.expect("an invite").code;
    let unreachable = SocketAddr::from((NOWHERE, host.port()));
    let invite = code_to_invite(&code, unreachable);
    assert_eq!(invite.hostname.as_deref(), Some(NAME));
    assert_eq!(invite.candidates.len(), 1);
    (host, invite)
}

// The host's address changed after the invite was made: nothing answers at
// the invite's one address, and the name's own nameserver knows the new one.
#[test]
fn client_finds_the_host_by_name() {
    let host_log = fresh_log("name", "found", "host");
    let client_log = fresh_log("name", "found", "client");
    let system = FakeSystem::new(Ipv4Addr::new(203, 0, 113, 77));
    let nameserver = FakeNameserver::start(Ipv4Addr::LOCALHOST);
    let tests_lookup = lookup(&system, nameserver.port, true);
    let (mut host, invite) = host_with_name(tests_lookup.clone(), host_log.clone());
    let host_port = host.port();
    let asked_by_host = nameserver.asked().len();

    let mut config = common::config("Ana", quick());
    config.lookup = tests_lookup;
    config.log = Some(client_log.clone());
    let joined = Instant::now();
    let mut client = Member::join_with(config, Arc::new(Identity::generate()), invite);
    let view = client.wait_for(Duration::from_secs(5), "client connected", connected);
    let asked = nameserver.asked();
    let first = *asked
        .get(asked_by_host)
        .expect("the client asked the nameserver");
    assert!(
        first >= joined + quick().still_trying_after,
        "the name was looked up {:?} after joining, before the fast round was over",
        first - joined
    );
    let name = view
        .numbers
        .address_name
        .expect("the client shows the name");
    assert_eq!(name.name, NAME);
    assert_eq!(
        name.answer,
        NameAnswer::Found {
            addrs: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            refused: Vec::new(),
        }
    );
    assert_eq!(name.outside, None);
    assert_eq!(
        view.numbers.peer_addr,
        Some(SocketAddr::from((Ipv4Addr::LOCALHOST, host_port)))
    );
    host.wait_for(Duration::from_secs(2), "the friend is in", |v| {
        v.people.len() == 2
    });
    // Asked directly, so the system resolver was never asked for NAME.
    assert!(
        !system
            .asked()
            .iter()
            .any(|asked| asked.contains(NAME) && !asked.starts_with("NS")),
        "{:?}",
        system.asked()
    );

    client.leave();
    host.leave();
    let host_text = read_log(&host_log);
    let client_text = read_log(&client_log);
    for want in [
        format!("address name {NAME}, which every invite carries"),
        format!("invite address name {NAME}"),
        format!("address name: looking up {NAME} to see whether it points to this pc"),
        format!("address name {NAME} points to 127.0.0.1; stun and the router have not said"),
    ] {
        assert!(host_text.contains(&want), "no {want:?} in\n{host_text}");
    }
    let server = SocketAddr::from((Ipv4Addr::LOCALHOST, nameserver.port));
    let at = SocketAddr::from((Ipv4Addr::LOCALHOST, host_port));
    for want in [
        format!(
            "the invite carries the address name {NAME}, looked up if nothing answers in 0.5 s"
        ),
        format!("address name: looking up {NAME}, since none of the invite's addresses answered"),
        format!("address name: asking the system resolver for the nameservers of {NAME}"),
        format!("address name: {NAME} has no nameservers of its own"),
        String::from("address name: example.net has nameservers ns1.example.net"),
        format!("address name: nameserver ns1.example.net is at {server}"),
        format!("address name: asking {server} for the A records of {NAME}, id "),
        format!("address name: A answer from {server} after "),
        String::from(" ms, authoritative: 127.0.0.1"),
        String::from("address name: took 127.0.0.1 (authoritative)"),
        format!("address name: trying {at} too"),
        format!("address name: the host answered at {at}, which only the name gave"),
        format!("connected to the host at {at}"),
    ] {
        assert!(client_text.contains(&want), "no {want:?} in\n{client_text}");
    }
}

// A real name can point at 127.0.0.1 as easily as a forged invite can.
// Without the test switch nothing is sent there: not the question to a
// nameserver on loopback, and not a handshake to the address it gives.
#[test]
fn name_pointing_at_loopback_is_refused() {
    let host_log = fresh_log("name", "refused", "host");
    let client_log = fresh_log("name", "refused", "client");
    let system = FakeSystem::new(Ipv4Addr::LOCALHOST);
    let nameserver = FakeNameserver::start(Ipv4Addr::LOCALHOST);
    let (mut host, invite) =
        host_with_name(lookup(&system, nameserver.port, true), host_log.clone());
    let asked_by_host = nameserver.asked().len();

    let mut config = common::config("Ana", quick());
    config.lookup = lookup(&system, nameserver.port, false);
    config.log = Some(client_log.clone());
    let mut client = Member::join_with(config, Arc::new(Identity::generate()), invite);
    let view = client.wait_for(Duration::from_secs(5), "the name looked up", |v| {
        found(v).is_some()
    });
    assert_eq!(
        found(&view),
        Some(NameAnswer::Found {
            addrs: Vec::new(),
            refused: vec![(IpAddr::V4(Ipv4Addr::LOCALHOST), "it is a loopback address")],
        })
    );
    client.holds_for(Duration::from_secs(1), "the client stays out", |v| {
        v.strip.state == LinkState::Connecting
    });
    assert_eq!(host.view().people.len(), 1, "a try reached the host");
    assert_eq!(
        nameserver.asked().len(),
        asked_by_host,
        "the client asked a nameserver on loopback"
    );
    assert!(
        system
            .asked()
            .contains(&format!("A {NAME}, cache bypassed")),
        "{:?}",
        system.asked()
    );

    client.leave();
    host.leave();
    let text = read_log(&client_log);
    let server = SocketAddr::from((Ipv4Addr::LOCALHOST, nameserver.port));
    for want in [
        format!(
            "address name: refused nameserver ns1.example.net at {server}: it is a loopback address"
        ),
        String::from(
            "address name: found nameservers for example.net but no address to ask them at",
        ),
        format!("address name: no nameservers of {NAME} to ask directly"),
        format!(
            "address name: asking the system resolver for the A records of {NAME}, bypassing its cache"
        ),
        String::from("address name: refused 127.0.0.1: it is a loopback address"),
    ] {
        assert!(text.contains(&want), "no {want:?} in\n{text}");
    }
    assert!(!text.contains("address name: trying"), "{text}");
    let hosted = read_log(&host_log);
    assert!(
        !hosted.contains("initiation"),
        "a try reached the host:\n{hosted}"
    );
}

// The host looks its own name up when the room opens and holds it against
// what STUN saw, so a dynamic DNS client that stopped updating shows.
#[test]
fn host_checks_its_own_name() {
    let stun = FakeStun::start(Some(Duration::ZERO));
    let outside = *OUTSIDE.ip();
    for (points_to, this_pc) in [(outside, true), (Ipv4Addr::new(198, 51, 100, 4), false)] {
        let system = FakeSystem::new(points_to);
        let mut config = common::config("Host", timers());
        config.stun_servers = vec![stun.addr.to_string()];
        config.address_name = Some(NAME.to_owned());
        // The real check: it refuses the nameserver on loopback, so the
        // system resolver answers, with public addresses the check lets by.
        config.lookup = lookup(&system, net::dns::PORT, false);
        let mut host = Member::host_with(config);
        let view = host.wait_for(Duration::from_secs(3), "the name checked", |v| {
            v.numbers
                .address_name
                .as_ref()
                .is_some_and(|name| name.outside.is_some())
        });
        let name = view.numbers.address_name.expect("the name");
        assert_eq!(name.outside, Some(NameMatch { points_to, outside }));
        assert_eq!(name.outside.unwrap().is_this_pc(), this_pc);
        host.leave();
    }
}
