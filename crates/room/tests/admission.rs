mod common;

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{Forwarder, Member, code_to_invite, host_invite, loopback, timers};
use keys::Identity;
use room::view::{LinkState, View};
use room::{Room, RoomError, Timers};

// Long enough for the ladder to try several times at the test timers' 100 ms.
const NEVER: Duration = Duration::from_millis(1500);

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn live(v: &View) -> bool {
    v.strip.state == LinkState::Live
}

fn connecting(v: &View) -> bool {
    v.strip.state == LinkState::Connecting && v.numbers.handshake_ms.is_none()
}

#[test]
fn single_use_invite_admits_one_key() {
    let host = Member::host("Host", timers());
    let invite = host_invite(&host);
    let first = Member::join("Ana", timers(), invite.clone());
    first.wait_for(Duration::from_secs(1), "first client live", live);
    host.wait_for(Duration::from_secs(1), "invite used", |v| {
        v.invite.as_ref().is_some_and(|i| i.used)
    });

    let second = Member::join("Bo", timers(), invite);
    second.holds_for(NEVER, "second key never connects", connecting);
    let hosted = host.view();
    assert_eq!(hosted.people.len(), 2, "{:#?}", hosted.people);
    assert!(hosted.numbers.dropped_bad > 0);
    first.holds_for(Duration::from_millis(100), "first client still live", live);
}

#[test]
fn same_key_may_come_back_with_its_single_use_invite() {
    let host = Member::host("Host", timers());
    let invite = host_invite(&host);
    let identity = Arc::new(Identity::generate());
    let mut first = Member::join_as(Arc::clone(&identity), "Ana", timers(), invite.clone());
    first.wait_for(Duration::from_secs(1), "client live", live);
    first.leave();
    host.wait_for(Duration::from_secs(1), "host alone", |v| {
        v.strip.state == LinkState::Alone
    });

    let again = Member::join_as(identity, "Ana", timers(), invite);
    again.wait_for(Duration::from_secs(1), "same key live again", live);
}

#[test]
fn multi_use_invite_admits_two() {
    let host = Member::host("Host", timers());
    host_invite(&host);
    host.room().new_invite(true);
    let view = host.wait_for(Duration::from_secs(1), "multi-use invite", |v| {
        v.invite
            .as_ref()
            .is_some_and(|i| i.multi_use && !i.code.is_empty())
    });
    let invite = code_to_invite(&view.invite.unwrap().code, loopback(host.port()));

    let a = Member::join("Ana", timers(), invite.clone());
    let b = Member::join("Bo", timers(), invite);
    a.wait_for(Duration::from_secs(1), "first client live", live);
    b.wait_for(Duration::from_secs(1), "second client live", live);
    let hosted = host.wait_for(Duration::from_secs(1), "three people", |v| {
        v.people.len() == 3
    });
    let mut names: Vec<&str> = hosted.people.iter().map(|p| p.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["Ana", "Bo", "Host"]);
    b.wait_for(
        Duration::from_secs(2),
        "roster reaches the second client",
        |v| v.people.len() == 3,
    );
}

#[test]
fn ninth_person_is_refused() {
    let host = Member::host("Host", timers());
    host_invite(&host);
    host.room().new_invite(true);
    let view = host.wait_for(Duration::from_secs(1), "multi-use invite", |v| {
        v.invite
            .as_ref()
            .is_some_and(|i| i.multi_use && !i.code.is_empty())
    });
    let invite = code_to_invite(&view.invite.unwrap().code, loopback(host.port()));

    let mut friends = Vec::new();
    for i in 0..7 {
        let friend = Member::join(&format!("Friend {i}"), timers(), invite.clone());
        friend.wait_for(Duration::from_secs(2), "friend live", live);
        friends.push(friend);
    }
    host.wait_for(Duration::from_secs(2), "eight people", |v| {
        v.people.len() == 8
    });
    friends[6].wait_for(Duration::from_secs(2), "full roster on a client", |v| {
        v.people.len() == 8
    });

    let ninth = Member::join("One too many", timers(), invite);
    ninth.holds_for(NEVER, "ninth never connects", connecting);
    assert_eq!(host.view().people.len(), 8);
}

#[test]
fn expired_invite_never_connects() {
    let short = Timers {
        invite_single_use_lifetime: Duration::from_millis(200),
        ..timers()
    };
    let host = Member::host("Host", short);
    let mut invite = host_invite(&host);
    host.wait_for(Duration::from_secs(2), "invite expired on the host", |v| {
        v.invite.as_ref().is_some_and(|i| i.expired)
    });

    // The client's own clock check would stop this before the host sees it.
    invite.expires_at = unix_now() + 600;
    let client = Member::join("Ana", timers(), invite.clone());
    client.holds_for(NEVER, "expired invite never connects", connecting);
    assert_eq!(host.view().people.len(), 1);

    invite.expires_at = unix_now() - 1;
    let notify: room::Notify = Arc::new(|| {});
    let config = common::config("Bo", timers());
    let dir = config.data_dir.clone();
    let refused = Room::join(config, Arc::new(Identity::generate()), invite, notify);
    assert!(matches!(refused, Err(RoomError::InviteExpired)));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tampered_secret_never_connects() {
    let host = Member::host("Host", timers());
    let mut invite = host_invite(&host);
    invite.secret[0] ^= 1;
    let client = Member::join("Ana", timers(), invite);
    client.holds_for(NEVER, "tampered invite never connects", connecting);
    assert_eq!(host.view().people.len(), 1);
    assert!(
        client.view().numbers.dropped_bad > 0,
        "the host answers, the answer fails"
    );
}

#[test]
fn wrong_host_key_gets_no_reply() {
    let host = Member::host("Host", timers());
    let forwarder = Forwarder::new(loopback(host.port()));
    let mut invite = common::invite_to(&host, forwarder.addr);
    invite.host_key = *Identity::generate().public();

    let client = Member::join("Ana", timers(), invite);
    client.holds_for(NEVER, "wrong host key never connects", connecting);
    assert!(forwarder.initiations().len() >= 5);
    assert_eq!(forwarder.packets_from_host(), 0, "the host answered");
    assert!(host.view().numbers.dropped_bad >= 5);
}
