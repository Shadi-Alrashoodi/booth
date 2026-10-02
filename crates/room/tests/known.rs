mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use common::{
    FakeStun, Folder, Forwarder, Member, OUTSIDE, StrictRouter, config_in, invite_to, loopback,
    timers,
};
use invite::{Answers, Candidate, CandidateKind, ReplyCode};
use keys::Identity;
use room::view::{LinkState, PasteState, ReplyState, View};
use room::{Candidates, KnownHost, Room, Timers};

fn live(v: &View) -> bool {
    v.strip.state == LinkState::Live
}

// A host and a friend who joined it once with an invite, both gone again,
// with what each kept in its own folder.
struct Before {
    host_dir: Folder,
    client_dir: Folder,
    host: Arc<Identity>,
    client: Arc<Identity>,
    port: u16,
    known: KnownHost,
}

// `through` is where the friend reaches the host, when not directly.
fn joined_once(test: &str, through: Option<&Forwarder>) -> Before {
    let host_dir = Folder::new(&format!("{test}-host"));
    let client_dir = Folder::new(&format!("{test}-client"));
    let host_identity = Arc::new(Identity::generate());
    let client_identity = Arc::new(Identity::generate());
    let mut host = Member::host_as(
        Arc::clone(&host_identity),
        config_in("Host", timers(), host_dir.0.clone()),
    );
    let port = host.port();
    let at = through.map_or(loopback(port), |forwarder| forwarder.addr);
    let invite = invite_to(&host, at);
    let mut client = Member::join_with(
        config_in("Ana", timers(), client_dir.0.clone()),
        Arc::clone(&client_identity),
        invite,
    );
    client.wait_for(Duration::from_secs(2), "client live", live);
    let hosted = host.wait_for(Duration::from_secs(2), "Ana in the room", |v| {
        v.people.len() == 2 && v.people[1].name == "Ana"
    });
    assert!(hosted.people[1].joined_by_invite);
    client.wait_for(Duration::from_secs(2), "the room's name", |v| {
        v.room_name == "Host's room"
    });
    client.leave();
    host.leave();

    let hosts = Room::known_hosts(&client_dir.0).expect("the list reads");
    assert_eq!(hosts.len(), 1);
    let known = hosts[0].clone();
    assert_eq!(known.host_key(), host_identity.public());
    assert_eq!(known.room_name(), "Host's room");
    assert_eq!(known.host_name(), "Host");
    let devices = Room::known_devices(&host_dir.0).expect("the list reads");
    assert_eq!(devices.devices.len(), 1);
    assert_eq!(devices.devices[0].key, *client_identity.public());
    assert_eq!(devices.devices[0].name, "Ana");
    Before {
        host_dir,
        client_dir,
        host: host_identity,
        client: client_identity,
        port,
        known,
    }
}

// The host starts again from the same folder, on the same port.
fn reopen(before: &Before, timers: Timers) -> Member {
    let mut config = config_in("Host", timers, before.host_dir.0.clone());
    config.port = before.port;
    Member::host_as(Arc::clone(&before.host), config)
}

fn rejoin(before: &Before, timers: Timers) -> Member {
    Member::rejoin_with(
        config_in("Ana", timers, before.client_dir.0.clone()),
        Arc::clone(&before.client),
        before.known.clone(),
    )
}

#[test]
fn known_friend_rejoins_without_an_invite() {
    let before = joined_once("rejoin", None);
    let host = reopen(&before, timers());
    let client = rejoin(&before, timers());
    client.wait_for(Duration::from_secs(2), "client live again", live);
    let hosted = host.wait_for(Duration::from_secs(2), "Ana back", |v| {
        v.people.len() == 2 && v.people[1].name == "Ana"
    });
    // Known from before, so no fingerprint this time.
    assert!(!hosted.people[1].joined_by_invite);
    assert_eq!(hosted.people[1].key, *before.client.public());
}

#[test]
fn removed_device_is_refused() {
    let before = joined_once("removed", None);
    Room::remove_device(&before.host_dir.0, before.client.public()).expect("removed");
    assert!(
        Room::known_devices(&before.host_dir.0)
            .expect("the list reads")
            .devices
            .is_empty()
    );
    let host = reopen(&before, timers());
    let client = rejoin(&before, timers());
    client.holds_for(Duration::from_millis(1500), "never connects", |v| {
        v.strip.state == LinkState::Connecting
    });
    assert_eq!(host.view().people.len(), 1);
    assert!(
        host.view().numbers.dropped_bad > 0,
        "the tries reached the host"
    );
}

// The host tells a friend its addresses once it has them, and again when
// they change; the friend keeps them for the next rejoin.
#[test]
fn friend_keeps_the_hosts_addresses() {
    let host_dir = Folder::new("addresses-host");
    let client_dir = Folder::new("addresses-client");
    let lan = Candidate {
        kind: CandidateKind::Lan,
        addr: "192.0.2.10:41000".parse().unwrap(),
    };
    // STUN answers only after the first invite is made.
    let stun = FakeStun::start(Some(Duration::from_millis(600)));
    let quick = Timers {
        first_invite_wait: Duration::from_millis(100),
        stun_wait: Duration::from_secs(2),
        ..timers()
    };
    let mut config = config_in("Host", quick, host_dir.0.clone());
    config.candidates = Candidates::Fixed(vec![lan]);
    config.stun_servers = vec![stun.addr.to_string()];
    let mut host = Member::host_with(config);
    let host_port = host.port();
    let mut invite = invite_to(&host, loopback(host_port));
    // Nothing is sent to the test network: only loopback is tried.
    assert!(invite.candidates.contains(&lan));
    assert!(
        invite
            .candidates
            .iter()
            .all(|c| c.kind != CandidateKind::Public)
    );
    invite.candidates.retain(|c| c.addr.ip().is_loopback());
    let mut client = Member::join_with(
        config_in("Ana", timers(), client_dir.0.clone()),
        Arc::new(Identity::generate()),
        invite,
    );
    client.wait_for(Duration::from_secs(2), "client live", live);
    host.wait_for(Duration::from_secs(3), "STUN answered", |v| {
        v.numbers.public_addr.is_some()
    });
    // The new addresses leave with the STUN answer; a moment to arrive.
    std::thread::sleep(Duration::from_millis(300));
    client.leave();
    host.leave();

    let hosts = Room::known_hosts(&client_dir.0).expect("the list reads");
    let public = |port: u16| Candidate {
        kind: CandidateKind::Public,
        addr: SocketAddr::from((*OUTSIDE.ip(), port)),
    };
    assert_eq!(
        hosts[0].candidates(),
        [lan, public(OUTSIDE.port()), public(host_port)]
    );
}

#[test]
fn rejoin_past_a_strict_router_with_a_code() {
    let (before, reached) = {
        // First through a plain forwarder, which is where the friend
        // reaches the host from then on.
        let host_dir = Folder::new("strict-host");
        let client_dir = Folder::new("strict-client");
        let host_identity = Arc::new(Identity::generate());
        let client_identity = Arc::new(Identity::generate());
        let mut host = Member::host_as(
            Arc::clone(&host_identity),
            config_in("Host", timers(), host_dir.0.clone()),
        );
        let port = host.port();
        let forwarder = Forwarder::new(loopback(port));
        let invite = invite_to(&host, forwarder.addr);
        let mut client = Member::join_with(
            config_in("Ana", timers(), client_dir.0.clone()),
            Arc::clone(&client_identity),
            invite,
        );
        client.wait_for(Duration::from_secs(2), "client live", live);
        client.leave();
        host.leave();
        let known = Room::known_hosts(&client_dir.0).expect("the list reads")[0].clone();
        let before = Before {
            host_dir,
            client_dir,
            host: host_identity,
            client: client_identity,
            port,
            known,
        };
        (before, forwarder.addr)
    };

    let quick = Timers {
        still_trying_after: Duration::from_millis(500),
        ..timers()
    };
    let host = reopen(&before, timers());
    // The same address the friend reached the host at, now a strict router.
    let router = StrictRouter::at(reached, loopback(before.port));
    let stun = FakeStun::start(Some(Duration::ZERO));
    let mut config = config_in("Ana", quick, before.client_dir.0.clone());
    config.stun_servers = vec![stun.addr.to_string()];
    let client = Member::rejoin_with(config, Arc::clone(&before.client), before.known.clone());

    let waiting = client.wait_for(Duration::from_secs(3), "the reply code", |v| {
        v.reply
            .as_ref()
            .is_some_and(|reply| matches!(reply.state, ReplyState::Code { .. }))
    });
    assert!(router.dropped() > 0, "no try reached the router");
    let reply = waiting.reply.expect("the reply screen");
    let mut code = ReplyCode::decode(&reply.code).expect("the code decodes");
    assert_eq!(code.answers, Answers::Rejoin);
    assert_eq!(code.client_key, *before.client.public());
    let SocketAddr::V4(inside) = router.inside else {
        panic!("the router is on IPv4 loopback");
    };
    code.outside_v4 = Some(inside);
    host.room()
        .accept_reply(code)
        .expect("the paste is accepted");

    client.wait_for(Duration::from_secs(2), "client live", live);
    assert!(router.is_open());
    let hosted = host.wait_for(Duration::from_secs(2), "the friend is back", |v| {
        v.people.len() == 2 && v.paste == Some(PasteState::Joined)
    });
    assert!(!hosted.people[1].joined_by_invite);
}
