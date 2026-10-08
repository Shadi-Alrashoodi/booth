mod common;

use std::time::Instant;

use common::*;
use session::{
    InitKind, PacketType, Session, SessionError, invite_psk, packet_type, read_initiation,
    response_receiver_index,
};

// A host session that has not heard from the client under the new keys.
fn assert_unconfirmed(host: &mut Session) {
    let mut wire = Vec::new();
    assert!(!host.is_confirmed());
    assert!(host.sealer().is_none());
    assert_eq!(
        host.encrypt(b"x", &mut wire),
        Err(SessionError::NotConfirmed)
    );
    assert!(wire.is_empty());
}

// A data packet with the right header and index and no keys behind it.
fn made_up_data(receiver_index: u32) -> Vec<u8> {
    let mut packet = vec![PacketType::Data.byte(), 0, 0, 0];
    packet.extend_from_slice(&receiver_index.to_le_bytes());
    packet.extend_from_slice(&0u64.to_le_bytes());
    packet.extend_from_slice(&[0xa5; 4 + 16]);
    packet
}

// IKpsk2 mixes the psk in only at the end of message 2. The host reads
// message 1, invite id and all, whatever psk the client used, and answers it.
// Only the first data packet under the new keys shows the client had the
// secret, so that is where a room may spend an invite or give out a seat.
#[test]
fn message_one_says_nothing_about_the_psk() {
    let peers = peers();
    let now = Instant::now();
    let right = invite_psk(&INVITE_SECRET, &peers.host.public, &peers.client.public);
    let guessed = invite_psk(&[0x43; 16], &peers.host.public, &peers.client.public);

    for client_psk in [[0; 32], [0x5a; 32], PEER_PSK, *guessed] {
        let (mut initiation, packet) = start(&peers, &client_psk, InitKind::Invite(INVITE_ID), 7);
        let incoming = read_initiation(&peers.host.private, &peers.host.public, &packet)
            .expect("read without the secret");
        assert_eq!(incoming.kind, InitKind::Invite(INVITE_ID));
        assert_eq!(incoming.remote_public, peers.client.public);
        assert_eq!(incoming.sender_index, 7);

        // The host answers before it can know whether the psk matches.
        let (mut host, response) = incoming.accept(&right, 9, now).expect("answered");
        assert_eq!(packet_type(&response), Some(PacketType::Response));
        assert_eq!(response_receiver_index(&response), Some(7));
        assert_unconfirmed(&mut host);

        // The sender cannot read the answer, so it has no keys to confirm with.
        assert_eq!(
            initiation.finish(&response, now).map(|_| ()),
            Err(SessionError::Handshake("could not decrypt response"))
        );
        // A data packet it makes up does not open either.
        let mut plain = Vec::new();
        assert_eq!(
            host.decrypt(&made_up_data(9), &mut plain),
            Err(SessionError::Decrypt)
        );
        assert_unconfirmed(&mut host);
    }
}

#[test]
fn first_data_packet_is_the_proof() {
    let peers = peers();
    let now = Instant::now();
    let psk = invite_psk(&INVITE_SECRET, &peers.host.public, &peers.client.public);
    let (mut initiation, packet) = start(&peers, &psk, InitKind::Invite(INVITE_ID), 7);
    let incoming = read_initiation(&peers.host.private, &peers.host.public, &packet).expect("read");
    assert_eq!(incoming.kind, InitKind::Invite(INVITE_ID));
    let (mut host, response) = incoming.accept(&psk, 9, now).expect("accept");
    let mut client = initiation.finish(&response, now).expect("finish");

    // The client holds the keys now, but the host has not seen it use them.
    assert_unconfirmed(&mut host);
    let mut plain = Vec::new();
    assert_eq!(
        host.decrypt(&made_up_data(9), &mut plain),
        Err(SessionError::Decrypt)
    );
    assert_unconfirmed(&mut host);

    let mut wire = Vec::new();
    client.encrypt(b"hello", &mut wire).expect("encrypt");
    host.decrypt(&wire, &mut plain).expect("decrypt");
    assert_eq!(plain, b"hello");
    assert!(host.is_confirmed());
    assert!(host.sealer().is_some());
    host.encrypt(b"welcome", &mut wire)
        .expect("host sends after confirmation");
    client.decrypt(&wire, &mut plain).expect("client decrypts");
    assert_eq!(plain, b"welcome");
}

// The invite psk is bound to both keys, so a psk made from the secret for one
// client key does not work under another.
#[test]
fn invite_psk_is_bound_to_the_client_key() {
    let peers = peers();
    let now = Instant::now();
    let stranger = keys();
    let for_client = invite_psk(&INVITE_SECRET, &peers.host.public, &peers.client.public);
    let for_stranger = invite_psk(&INVITE_SECRET, &peers.host.public, &stranger.public);
    assert_ne!(*for_client, *for_stranger);

    let (mut initiation, packet) = start(&peers, &for_stranger, InitKind::Invite(INVITE_ID), 1);
    let incoming = read_initiation(&peers.host.private, &peers.host.public, &packet).expect("read");
    assert_eq!(incoming.remote_public, peers.client.public);
    let host_psk = invite_psk(&INVITE_SECRET, &peers.host.public, &incoming.remote_public);
    assert_eq!(*host_psk, *for_client);
    let (mut host, response) = incoming.accept(&host_psk, 2, now).expect("accept");
    assert_eq!(
        initiation.finish(&response, now).map(|_| ()),
        Err(SessionError::Handshake("could not decrypt response"))
    );
    assert_unconfirmed(&mut host);
}
