mod common;

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use common::{FakeStun, Member, OUTSIDE, StrictRouter, invite_to, loopback, timers};
use invite::{Answers, ReplyCode};
use keys::Identity;
use room::Timers;
use room::view::{LinkState, PasteState, ReplyState, View};

fn live(v: &View) -> bool {
    v.strip.state == LinkState::Live
}

fn has_code(v: &View) -> bool {
    v.reply
        .as_ref()
        .is_some_and(|reply| matches!(reply.state, ReplyState::Code { .. }))
}

fn fresh_log(side: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("booth-reply-{}-{side}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("make a folder for the log");
    dir.join("booth.log")
}

#[test]
fn reply_code_gets_past_a_strict_router() {
    let host_log = fresh_log("host");
    let client_log = fresh_log("client");
    let mut host_config = common::config("Host", timers());
    host_config.log = Some(host_log.clone());
    let mut host = Member::host_with(host_config);
    let router = StrictRouter::new(loopback(host.port()));
    let stun = FakeStun::start(Some(Duration::ZERO));
    let quick = Timers {
        still_trying_after: Duration::from_millis(500),
        ..timers()
    };
    let mut config = common::config("Ana", quick);
    config.stun_servers = vec![stun.addr.to_string()];
    config.log = Some(client_log.clone());
    let invite = invite_to(&host, router.outside);
    let invite_id = invite.invite_id;
    let mut client = Member::join_with(config, Arc::new(Identity::generate()), invite);

    let joining = client.wait_for(Duration::from_secs(3), "the reply code", has_code);
    assert_eq!(joining.strip.state, LinkState::Connecting);
    assert!(router.dropped() > 0, "no try reached the router");
    assert!(!router.is_open());
    assert_eq!(host.view().people.len(), 1, "a try got past the router");

    let reply = joining.reply.expect("the reply screen");
    let mut code = ReplyCode::decode(&reply.code).expect("the code decodes");
    assert_eq!(code.answers, Answers::Invite(invite_id));
    assert_eq!(code.client_key, *client.identity.public());
    assert_eq!(code.outside_v4, Some(OUTSIDE));
    // The text format refuses loopback, so the struct gets the address the
    // host's side sees the client at.
    let SocketAddr::V4(inside) = router.inside else {
        panic!("the router is on IPv4 loopback");
    };
    code.outside_v4 = Some(inside);
    let accepted = host
        .room()
        .accept_reply(code)
        .expect("the paste is accepted");
    assert_eq!(accepted.to, [router.inside]);

    client.wait_for(Duration::from_secs(2), "client live", live);
    assert!(router.is_open());
    let hosted = host.wait_for(Duration::from_secs(2), "the friend is in", |v| {
        v.people.len() == 2 && v.paste == Some(PasteState::Joined)
    });
    assert_eq!(hosted.people[1].key, *client.identity.public());
    assert_eq!(client.view().reply, None);

    client.leave();
    host.leave();
    let host_text = fs::read_to_string(&host_log).expect("the host wrote a log");
    let client_text = fs::read_to_string(&client_log).expect("the client wrote a log");
    let fingerprint = client.identity.fingerprint();
    for want in [
        format!("reply code pasted for {fingerprint}: answers an invite, outside {inside}"),
        String::from("reply code checks passed: not expired"),
        format!("punch round 1 of 10 for {fingerprint} sent to {inside}"),
        format!("{fingerprint} joined after "),
    ] {
        assert!(host_text.contains(&want), "no {want:?} in\n{host_text}");
    }
    for want in [
        format!("stun request sent to {}", stun.addr),
        format!("stun answer from {}: seen as {OUTSIDE}", stun.addr),
        String::from("first stun round: mapping unknown"),
        String::from("the panel says it is still trying"),
        format!("reply code made: this pc's mapping unknown, outside {OUTSIDE}"),
        format!(
            "the host's punch packets are arriving from {}",
            router.outside
        ),
        String::from("the reply code screen closes, the host answered"),
    ] {
        assert!(client_text.contains(&want), "no {want:?} in\n{client_text}");
    }
    // It names the friend by fingerprint and address, and the text of the
    // code stays out, as the invite's does.
    assert!(!client_text.contains(&reply.code));
    for log in [host_log, client_log] {
        let _ = fs::remove_dir_all(log.parent().expect("a folder"));
    }
}
