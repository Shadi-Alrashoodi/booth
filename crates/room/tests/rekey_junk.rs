mod common;

use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use common::{Member, host_invite, loopback, timers};
use keys::Identity;
use room::Timers;
use room::view::LinkState;
use session::{InitKind, Initiation, TimestampSource};

// On loopback every member has the same IP, as friends behind one NAT or
// CGNAT address do. Junk from that IP and another port, which needs no key
// and no mac1, must not spend the tokens a friend's rekeys need.
#[test]
fn junk_from_a_friends_ip_does_not_stop_its_rekeys() {
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
    let before = host.view().numbers.rekeys;

    let stop = Arc::new(AtomicBool::new(false));
    let junk = {
        let stop = Arc::clone(&stop);
        let to = loopback(host.port());
        thread::spawn(move || {
            let socket = UdpSocket::bind(loopback(0)).expect("bind the junk socket");
            // The whole burst at once, then faster than the bucket fills.
            for _ in 0..30 {
                let _ = socket.send_to(&[0x11], to);
            }
            while !stop.load(Ordering::Acquire) {
                let _ = socket.send_to(&[0x11], to);
                thread::sleep(Duration::from_millis(2));
            }
        })
    };

    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end {
        client.holds_for(Duration::from_millis(50), "client stays live", |v| {
            v.strip.state == LinkState::Live
        });
    }
    stop.store(true, Ordering::Release);
    junk.join().expect("junk thread");

    let rekeys = host.view().numbers.rekeys - before;
    assert!(rekeys >= 4, "only {rekeys} rekeys reached the host");
}

// A friend who kept the invite has the host key, so it can send initiations
// that pass mac1 without finishing a handshake. A flood of them from a
// member's IP keeps the host under load, where it wants a cookie back in
// mac2 before any key math. The cookie a friend needs must still get through
// from that same IP, so the friend keeps rekeying and stays Live.
#[test]
fn mac1_junk_under_load_does_not_stop_a_friends_rekeys() {
    // reject_after leaves room for the first rekey, which has to learn a
    // cookie: the host answers the try with one and the retry a second later
    // carries it back.
    let short = Timers {
        rekey_after: Duration::from_millis(300),
        reject_after: Duration::from_secs(2),
        ..timers()
    };
    let host = Member::host("Host", short);
    let invite = host_invite(&host);
    let client = Member::join("Ana", short, invite.clone());
    client.wait_for(Duration::from_secs(1), "client live", |v| {
        v.strip.state == LinkState::Live
    });
    let before = host.view().numbers.rekeys;

    // One initiation that passes mac1, built from the host key the invite
    // carries. Its invite id is one the host never gave out, so it is dropped
    // before it could become a peer, and it carries no cookie, so a host
    // under load only ever answers it with one.
    let flooder = Identity::generate();
    let (_, packet) = Initiation::start(
        &flooder.private_bytes(),
        flooder.public(),
        &invite.host_key,
        &[0u8; 32],
        InitKind::Invite([0x22; 8]),
        TimestampSource::new().next_stamp(),
        1,
    )
    .expect("build a mac1-valid initiation");

    let stop = Arc::new(AtomicBool::new(false));
    let junk = {
        let stop = Arc::clone(&stop);
        let to = loopback(host.port());
        thread::spawn(move || {
            let socket = UdpSocket::bind(loopback(0)).expect("bind the junk socket");
            // Enough at once to put the host under load, then faster than the
            // per-IP bucket fills so it stays drained.
            for _ in 0..50 {
                let _ = socket.send_to(&packet, to);
            }
            while !stop.load(Ordering::Acquire) {
                let _ = socket.send_to(&packet, to);
                thread::sleep(Duration::from_millis(2));
            }
        })
    };

    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end {
        client.holds_for(Duration::from_millis(50), "client stays live", |v| {
            v.strip.state == LinkState::Live
        });
    }
    stop.store(true, Ordering::Release);
    junk.join().expect("junk thread");

    // Still Live past reject_after means it kept rekeying while the flood
    // held the host under load.
    let rekeys = host.view().numbers.rekeys - before;
    assert!(rekeys >= 2, "only {rekeys} rekeys reached the host");
}
