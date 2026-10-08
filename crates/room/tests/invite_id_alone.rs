mod common;

use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use common::{Member, code_to_invite, host_invite, loopback, timers, wake};
use invite::Invite;
use keys::Identity;
use room::view::{LinkState, View};
use session::{InitKind, Initiation, PacketType, TimestampSource};

fn live(v: &View) -> bool {
    v.strip.state == LinkState::Live
}

// One initiation on the invite from a socket of its own, with the stranger's
// key and a psk it can only guess: what someone with the host key and the
// invite id, which a reply code carries, can send without the secret. IKpsk2
// mixes the psk in at the end of message 2, so the host cannot tell yet and
// answers message 1 all the same. The stranger cannot finish with that
// answer. The socket blocks with no timeout, as the fakes in common do, and
// wake() ends the read when no answer came.
fn knock(stranger: &Identity, invite: &Invite, host: SocketAddr) {
    let (mut initiation, packet) = Initiation::start(
        &stranger.private_bytes(),
        stranger.public(),
        &invite.host_key,
        &[0; 32],
        InitKind::Invite(invite.invite_id),
        TimestampSource::new().next_stamp(),
        9,
    )
    .expect("start an initiation");
    let socket = Arc::new(UdpSocket::bind(loopback(0)).expect("bind the stranger's socket"));
    let (tx, replies) = mpsc::channel();
    let reader = thread::spawn({
        let socket = Arc::clone(&socket);
        move || {
            let mut buf = [0u8; 256];
            if let Ok((len, _)) = socket.recv_from(&mut buf) {
                let _ = tx.send(buf[..len].to_vec());
            }
        }
    });
    socket.send_to(&packet, host).expect("send the initiation");
    let reply = replies.recv_timeout(Duration::from_secs(2));
    wake(&socket);
    let _ = reader.join();
    let reply = reply.expect("the host answers message 1");
    assert_eq!(session::packet_type(&reply), Some(PacketType::Response));
    assert!(
        initiation.finish(&reply, Instant::now()).is_err(),
        "the stranger finished the handshake without the secret"
    );
}

// The answer to message 1 must not spend a single-use invite: the friend it
// was made for still gets in.
#[test]
fn invite_id_without_the_secret_does_not_spend_the_invite() {
    let host = Member::host("Host", timers());
    let invite = host_invite(&host);
    knock(&Identity::generate(), &invite, loopback(host.port()));

    let friend = Member::join("Ana", timers(), invite);
    friend.wait_for(
        Duration::from_secs(2),
        "the friend holding the secret gets in",
        live,
    );
    host.wait_for(Duration::from_secs(1), "invite used", |v| {
        v.invite.as_ref().is_some_and(|i| i.used)
    });
    host.wait_for(Duration::from_secs(1), "two people", |v| {
        v.people.len() == 2
    });
}

// A multi-use invite lets in many keys, so on its id alone a stranger can
// have one answered for each of the room's seven seats. Those answers wait
// 5 s for a confirm that never comes and must not hold the seats meanwhile:
// the friend holding the secret is given less than that to get in.
#[test]
fn invite_id_without_the_secret_holds_no_seat() {
    let host = Member::host("Host", timers());
    host_invite(&host);
    host.room().new_invite(true);
    let view = host.wait_for(Duration::from_secs(1), "multi-use invite", |v| {
        v.invite
            .as_ref()
            .is_some_and(|i| i.multi_use && !i.code.is_empty())
    });
    let host_addr = loopback(host.port());
    let invite = code_to_invite(&view.invite.unwrap().code, host_addr);
    for _ in 0..7 {
        knock(&Identity::generate(), &invite, host_addr);
    }

    let friend = Member::join("Ana", timers(), invite);
    friend.wait_for(
        Duration::from_secs(2),
        "the friend holding the secret gets a seat",
        live,
    );
    host.wait_for(Duration::from_secs(1), "two people", |v| {
        v.people.len() == 2
    });
}
