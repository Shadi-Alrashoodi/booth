mod common;

use std::fs;
use std::net::UdpSocket;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use common::{Member, config, loopback, timers};
use keys::Identity;
use room::view::LinkState;

fn fresh_log(side: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("booth-log-{}-{side}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("make a folder for the log");
    dir.join("booth.log")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn logged_join_keeps_the_secrets() {
    let host_log = fresh_log("host");
    let client_log = fresh_log("client");
    let mut host_config = config("Mara", timers());
    host_config.log = Some(host_log.clone());
    let mut host = Member::host_with(host_config);

    // What a friend testing the port from outside would send.
    let probe = UdpSocket::bind(loopback(0)).expect("bind the probe");
    let probe_from = probe.local_addr().expect("probe address");
    probe
        .send_to(b"booth port test", loopback(host.port()))
        .expect("send the probe");

    let view = host.wait_for(Duration::from_secs(2), "invite code", |v| {
        v.invite.as_ref().is_some_and(|i| !i.code.is_empty())
    });
    let code = view.invite.expect("an invite").code;
    let invite = common::code_to_invite(&code, loopback(host.port()));
    let secrets = [hex(&invite.invite_id), hex(&invite.secret)];
    let client_identity = Arc::new(Identity::generate());
    let keys = [hex(host.identity.public()), hex(client_identity.public())];

    let mut client_config = config("Ana", timers());
    client_config.log = Some(client_log.clone());
    let mut client = Member::join_with(client_config, client_identity, invite.clone());
    client.wait_for(Duration::from_secs(5), "connected", |v| {
        v.strip.state == LinkState::Live && v.numbers.connect_ms.is_some()
    });
    // The writer is stopped inside leave(), which the panel waits on.
    let took = client.leave();
    assert!(
        took < Duration::from_millis(200),
        "client leave took {took:?}"
    );
    let took = host.leave();
    assert!(
        took < Duration::from_millis(200),
        "host leave took {took:?}"
    );

    let host_text = fs::read_to_string(&host_log).expect("the host wrote a log");
    let client_text = fs::read_to_string(&client_log).expect("the client wrote a log");
    let probe_line = format!("from {probe_from}, 15 bytes: not a booth packet (first byte 0x62)");
    for want in [
        " host   booth ",
        probe_line.as_str(),
        "invite made, single use",
        "initiation (invite) from ",
        ", answered",
        "session confirmed (invite) at 127.0.0.1:",
        "is called \"Ana\"",
        "left, said bye",
        "room closed",
    ] {
        assert!(host_text.contains(want), "no {want:?} in\n{host_text}");
    }
    for want in [
        " client joining host ",
        "invite initiation sent to 127.0.0.1:",
        "invite answer taken",
        "connected to the host at 127.0.0.1:",
        "leaving, bye sent to the host",
    ] {
        assert!(client_text.contains(want), "no {want:?} in\n{client_text}");
    }

    for text in [&host_text, &client_text] {
        assert!(text.is_ascii());
        assert!(!text.contains(&code), "the invite code is in\n{text}");
        for secret in secrets.iter().chain(&keys) {
            assert!(!text.contains(secret.as_str()), "{secret} is in\n{text}");
        }
    }
    for log in [host_log, client_log] {
        let _ = fs::remove_dir_all(log.parent().expect("a folder"));
    }
}
