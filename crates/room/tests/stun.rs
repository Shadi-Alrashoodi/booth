mod common;

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::thread;
use std::time::{Duration, Instant};

use common::{FakeStun, Member, OUTSIDE, timers};
use invite::{Candidate, CandidateKind, Invite, Mapping};
use room::Timers;
use room::view::{MappingWord, RouterState};

fn host_asking(fake: &FakeStun, timers: Timers) -> Member {
    host_asking_all(&[fake], timers)
}

fn host_asking_all(fakes: &[&FakeStun], timers: Timers) -> Member {
    let mut config = common::config("Host", timers);
    config.stun_servers = fakes.iter().map(|fake| fake.addr.to_string()).collect();
    Member::host_with(config)
}

fn public_candidates(host: &Member) -> Vec<SocketAddr> {
    let view = host.wait_for(Duration::from_secs(2), "invite code", |v| {
        v.invite.as_ref().is_some_and(|i| !i.code.is_empty())
    });
    let invite = Invite::decode(&view.invite.unwrap().code).unwrap();
    assert!(
        invite
            .candidates
            .iter()
            .all(|c| c.kind == CandidateKind::Public),
        "{:?}",
        invite.candidates
    );
    invite.candidates.iter().map(|c| c.addr).collect()
}

fn on_outside(port: u16) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(*OUTSIDE.ip(), port))
}

// The user's router: both servers see the same outside port, and it is not
// the one the host is bound to, where the forward leads.
#[test]
fn easy_mapping_gives_what_stun_saw_and_the_bound_port() {
    let one = FakeStun::on(Ipv4Addr::LOCALHOST, Some(Duration::ZERO), |_| OUTSIDE);
    let two = FakeStun::on(Ipv4Addr::new(127, 0, 0, 2), Some(Duration::ZERO), |_| {
        OUTSIDE
    });
    let short = Timers {
        stun_wait: Duration::from_secs(1),
        ..timers()
    };
    let host = host_asking_all(&[&one, &two], short);
    let publics = public_candidates(&host);
    assert_eq!(host.view().numbers.mapping, Some(MappingWord::Easy));
    let mut want = vec![SocketAddr::V4(OUTSIDE)];
    if host.port() != OUTSIDE.port() {
        want.push(on_outside(host.port()));
    }
    assert_eq!(publics, want);
}

#[test]
fn hard_mapping_leaves_only_the_bound_port() {
    let one = FakeStun::answering(Some(Duration::ZERO), |_| OUTSIDE);
    let other = SocketAddrV4::new(*OUTSIDE.ip(), OUTSIDE.port() + 1);
    let two = FakeStun::answering(Some(Duration::ZERO), move |_| other);
    let short = Timers {
        stun_wait: Duration::from_secs(1),
        ..timers()
    };
    let host = host_asking_all(&[&one, &two], short);
    let publics = public_candidates(&host);
    assert_eq!(host.view().numbers.mapping, Some(MappingWord::Hard));
    assert_eq!(publics, vec![on_outside(host.port())]);
}

// A router that keeps the port shows STUN the bound port itself, and the
// forwarded candidate would only repeat it.
#[test]
fn a_kept_port_goes_in_once() {
    let fake = FakeStun::answering(Some(Duration::ZERO), |from| {
        SocketAddrV4::new(*OUTSIDE.ip(), from.port())
    });
    let host = host_asking(&fake, timers());
    let publics = public_candidates(&host);
    assert_eq!(publics, vec![on_outside(host.port())]);
}

#[test]
fn first_invite_waits_for_the_stun_answer() {
    let fake = FakeStun::start(Some(Duration::from_millis(150)));
    let short = Timers {
        stun_wait: Duration::from_secs(3),
        stun_every: Duration::from_millis(300),
        ..timers()
    };
    let started = Instant::now();
    let host = host_asking(&fake, short);
    let testing = host.view().invite.expect("an invite slot while testing");
    assert_eq!(testing.router, RouterState::Testing);
    assert!(testing.code.is_empty());

    let view = host.wait_for(Duration::from_secs(2), "invite code", |v| {
        v.invite.as_ref().is_some_and(|i| !i.code.is_empty())
    });
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(150) && took < Duration::from_secs(1),
        "the answer came after 150 ms and the wait is 3 s, the invite took {took:?}"
    );
    let shown = view.invite.unwrap();
    // One server cannot tell easy mapping from hard.
    assert_eq!(shown.router, RouterState::Unknown);
    assert_eq!(view.numbers.mapping, Some(MappingWord::Unknown));
    assert_eq!(view.numbers.public_addr, Some(SocketAddr::V4(OUTSIDE)));

    // What STUN saw, then the same address with the port the host is bound
    // to, which is where a port forwarded by hand leads. Windows picks that
    // port, and once in a long while it picks 52000.
    let invite = Invite::decode(&shown.code).unwrap();
    assert_eq!(invite.mapping, Mapping::Unknown);
    let mut want = vec![Candidate {
        kind: CandidateKind::Public,
        addr: SocketAddr::V4(OUTSIDE),
    }];
    if host.port() != OUTSIDE.port() {
        want.push(Candidate {
            kind: CandidateKind::Public,
            addr: on_outside(host.port()),
        });
    }
    assert_eq!(invite.candidates, want);

    let deadline = Instant::now() + Duration::from_secs(2);
    while fake.requests() < 3 {
        assert!(
            Instant::now() < deadline,
            "stun is not repeated: {} requests",
            fake.requests()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn silent_stun_server_still_gives_an_invite() {
    let fake = FakeStun::start(None);
    let short = Timers {
        stun_wait: Duration::from_millis(300),
        ..timers()
    };
    let started = Instant::now();
    let host = host_asking(&fake, short);
    let view = host.wait_for(Duration::from_secs(2), "invite code", |v| {
        v.invite.as_ref().is_some_and(|i| !i.code.is_empty())
    });
    assert!(started.elapsed() >= Duration::from_millis(300));
    // No answer and no mapping: the invite carries no outside address, and
    // the panel says so with its fix.
    assert_eq!(view.invite.unwrap().router, RouterState::NoAddress);
    assert_eq!(view.numbers.public_addr, None);
    assert!(fake.requests() >= 1);
}

#[test]
fn leave_does_not_wait_for_a_name_lookup() {
    let mut config = common::config("Host", timers());
    // Windows sends a single label to LLMNR and NetBIOS, which take about a
    // second to give up on it.
    config.stun_servers = vec!["booth-no-such-stun-server".to_owned()];
    let mut host = Member::host_with(config);
    let took = host.leave();
    assert!(took < Duration::from_millis(200), "leave took {took:?}");
}

#[test]
fn invite_asked_for_while_testing_comes_out_multi_use() {
    let fake = FakeStun::start(Some(Duration::from_millis(100)));
    let short = Timers {
        stun_wait: Duration::from_secs(3),
        ..timers()
    };
    let host = host_asking(&fake, short);
    host.room().new_invite(true);
    let view = host.wait_for(Duration::from_secs(2), "invite code", |v| {
        v.invite.as_ref().is_some_and(|i| !i.code.is_empty())
    });
    let shown = view.invite.unwrap();
    assert!(shown.multi_use);
    assert!(Invite::decode(&shown.code).unwrap().multi_use);
}
