mod common;

use std::time::{Duration, Instant};

use common::*;
use session::{
    InitKind, Initiation, PROLOGUE, PacketType, SessionError, Tai64N, invite_psk, packet_type,
    read_initiation, response_receiver_index,
};

#[test]
fn first_join_by_invite() {
    let peers = peers();
    let now = Instant::now();
    let stamp = Tai64N::now();
    let client_psk = invite_psk(&INVITE_SECRET, &peers.host.public, &peers.client.public);
    let (mut initiation, packet) = Initiation::start(
        &peers.client.private,
        &peers.client.public,
        &peers.host.public,
        &client_psk,
        InitKind::Invite(INVITE_ID),
        stamp,
        0xdead_beef,
    )
    .expect("start");
    assert_eq!(initiation.sender_index(), 0xdead_beef);
    assert_eq!(packet_type(&packet), Some(PacketType::Initiation));

    let incoming = read_initiation(&peers.host.private, &peers.host.public, &packet).expect("read");
    assert_eq!(incoming.remote_public, peers.client.public);
    assert_eq!(incoming.kind, InitKind::Invite(INVITE_ID));
    assert_eq!(incoming.timestamp, stamp);
    assert_eq!(incoming.sender_index, 0xdead_beef);

    let host_psk = invite_psk(&INVITE_SECRET, &peers.host.public, &incoming.remote_public);
    let (mut host, response) = incoming.accept(&host_psk, 77, now).expect("accept");
    assert_eq!(packet_type(&response), Some(PacketType::Response));
    assert_eq!(response_receiver_index(&response), Some(0xdead_beef));
    assert!(
        response.len() < packet.len(),
        "the host must never send more bytes than it received before the handshake completes"
    );

    let mut client = initiation.finish(&response, now).expect("finish");
    assert_eq!(client.local_index(), 0xdead_beef);
    assert_eq!(client.remote_index(), 77);
    assert_eq!(host.local_index(), 77);
    assert_eq!(host.remote_index(), 0xdead_beef);
    assert_eq!(client.remote_public(), peers.host.public);
    assert_eq!(host.remote_public(), peers.client.public);
    assert!(client.is_initiator());
    assert!(!host.is_initiator());
    assert!(client.is_confirmed());
    assert!(!host.is_confirmed());
    assert_eq!(client.created(), now);

    let mut wire = Vec::new();
    let mut plain = Vec::new();
    assert_eq!(
        host.encrypt(b"too early", &mut wire),
        Err(SessionError::NotConfirmed)
    );
    assert!(wire.is_empty());

    client
        .encrypt(b"hello host", &mut wire)
        .expect("client encrypts");
    assert_eq!(packet_type(&wire), Some(PacketType::Data));
    host.decrypt(&wire, &mut plain).expect("host decrypts");
    assert_eq!(plain, b"hello host");
    assert!(host.is_confirmed());

    host.encrypt(b"hello client", &mut wire)
        .expect("host encrypts");
    client.decrypt(&wire, &mut plain).expect("client decrypts");
    assert_eq!(plain, b"hello client");
}

#[test]
fn rejoin_as_a_known_peer() {
    let peers = peers();
    let now = Instant::now();
    let (mut initiation, packet) = start(&peers, &PEER_PSK, InitKind::Known, 5);
    let incoming = read_initiation(&peers.host.private, &peers.host.public, &packet).expect("read");
    assert_eq!(incoming.kind, InitKind::Known);
    assert_eq!(incoming.remote_public, peers.client.public);
    let (_host, response) = incoming.accept(&PEER_PSK, 6, now).expect("accept");
    let client = initiation.finish(&response, now).expect("finish");
    assert_eq!(client.remote_index(), 6);
}

#[test]
fn rekey_reaches_the_host_as_rekey() {
    let peers = peers();
    let now = Instant::now();
    let (mut initiation, packet) = start(&peers, &PEER_PSK, InitKind::Rekey, 7);
    let incoming = read_initiation(&peers.host.private, &peers.host.public, &packet).expect("read");
    assert_eq!(incoming.kind, InitKind::Rekey);
    let (_host, response) = incoming.accept(&PEER_PSK, 8, now).expect("accept");
    initiation.finish(&response, now).expect("finish");
}

#[test]
fn initiation_lengths() {
    let peers = peers();
    let (_, known) = start(&peers, &PEER_PSK, InitKind::Known, 1);
    let (_, invite) = start(&peers, &PEER_PSK, InitKind::Invite(INVITE_ID), 1);
    assert_eq!(known.len(), 150);
    assert_eq!(invite.len(), 158);
    assert!(known.ends_with(&[0; 16]), "mac2 is sent as zeros");
    assert!(invite.ends_with(&[0; 16]), "mac2 is sent as zeros");
}

#[test]
fn wrong_psk_fails() {
    let peers = peers();
    let now = Instant::now();
    let (mut initiation, packet) = start(&peers, &PEER_PSK, InitKind::Known, 1);
    let incoming = read_initiation(&peers.host.private, &peers.host.public, &packet).expect("read");
    let (_host, response) = incoming.accept(&[0x99; 32], 2, now).expect("accept");
    assert_eq!(
        initiation.finish(&response, now).map(|_| ()),
        Err(SessionError::Handshake("could not decrypt response"))
    );

    let invite_for_someone_else = invite_psk(&INVITE_SECRET, &peers.host.public, &[5; 32]);
    let (mut initiation, packet) = start(
        &peers,
        &invite_for_someone_else,
        InitKind::Invite(INVITE_ID),
        1,
    );
    let incoming = read_initiation(&peers.host.private, &peers.host.public, &packet).expect("read");
    let right = invite_psk(&INVITE_SECRET, &peers.host.public, &incoming.remote_public);
    let (_host, response) = incoming.accept(&right, 2, now).expect("accept");
    assert!(initiation.finish(&response, now).is_err());
}

#[test]
fn wrong_host_key_fails_at_mac1() {
    let peers = peers();
    let stranger = keys();
    let (_, packet) = Initiation::start(
        &peers.client.private,
        &peers.client.public,
        &stranger.public,
        &PEER_PSK,
        InitKind::Known,
        Tai64N::now(),
        1,
    )
    .expect("start");
    assert_eq!(
        read_initiation(&peers.host.private, &peers.host.public, &packet).map(|_| ()),
        Err(SessionError::BadMac1)
    );
}

#[test]
fn right_host_key_on_the_wrong_host_fails() {
    let peers = peers();
    let (_, packet) = start(&peers, &PEER_PSK, InitKind::Known, 1);
    // Right public key for mac1, wrong private key: the Noise part cannot be read.
    let other = keys();
    assert_eq!(
        read_initiation(&other.private, &peers.host.public, &packet).map(|_| ()),
        Err(SessionError::Handshake("could not decrypt initiation"))
    );
}

// mac2, the last 16 bytes, is not checked; mac2_is_ignored_on_receipt covers it.
#[test]
fn flipped_initiation_bytes() {
    let peers = peers();
    for kind in [
        InitKind::Known,
        InitKind::Rekey,
        InitKind::Invite(INVITE_ID),
    ] {
        let (_, packet) = start(&peers, &PEER_PSK, kind, 1);
        read_initiation(&peers.host.private, &peers.host.public, &packet).expect("original reads");
        for at in 0..packet.len() - 16 {
            for bits in [0x01, 0x80, 0xff] {
                let changed = flipped(&packet, at, bits);
                assert!(
                    read_initiation(&peers.host.private, &peers.host.public, &changed).is_err(),
                    "byte {at} xor {bits:#x} was accepted"
                );
            }
        }
    }
}

#[test]
fn flipped_response_bytes() {
    let peers = peers();
    let now = Instant::now();
    let (mut initiation, packet) = start(&peers, &PEER_PSK, InitKind::Known, 1);
    let (_host, response) = respond(&peers, &packet, 2, now);
    // The same initiation takes every damaged copy, which also shows none of them spends it.
    for at in 0..response.len() - 16 {
        for bits in [0x01, 0x80, 0xff] {
            let changed = flipped(&response, at, bits);
            assert!(
                initiation.finish(&changed, now).is_err(),
                "byte {at} xor {bits:#x} was accepted"
            );
        }
    }
    initiation
        .finish(&response, now)
        .expect("the undamaged response still finishes");
}

#[test]
fn mac2_is_ignored_on_receipt() {
    let peers = peers();
    let now = Instant::now();
    let (mut initiation, mut packet) = start(&peers, &PEER_PSK, InitKind::Known, 1);
    let len = packet.len();
    packet[len - 16..].fill(0xee);
    let incoming = read_initiation(&peers.host.private, &peers.host.public, &packet)
        .expect("an initiation with a non-zero mac2 reads");
    let (_host, mut response) = incoming.accept(&PEER_PSK, 2, now).expect("accept");
    let len = response.len();
    response[len - 16..].fill(0xee);
    assert_eq!(response_receiver_index(&response), Some(1));
    initiation
        .finish(&response, now)
        .expect("a response with a non-zero mac2 finishes");
}

#[test]
fn failed_responses_leave_the_initiation_usable() {
    let peers = peers();
    let now = Instant::now();
    let (mut initiation, packet) = start(&peers, &PEER_PSK, InitKind::Known, 1);
    let (mut host, genuine) = respond(&peers, &packet, 2, now);

    // The client's index is in the clear in its initiation, so anyone who saw that can aim junk
    // at it. Response layout: header 0..4, sender 4..8, receiver 8..12, ephemeral 12..44,
    // tag 44..60, mac1 60..76, mac2 76..92.
    let mut junk = vec![0x12, 0, 0, 0];
    junk.extend_from_slice(&9u32.to_le_bytes());
    junk.extend_from_slice(&1u32.to_le_bytes());
    junk.resize(genuine.len(), 0xa5);
    let mut junk_past_mac1 = junk.clone();
    remac(&mut junk_past_mac1, &peers.client.public);
    let changed = |at: std::ops::Range<usize>, value: u8| {
        let mut response = genuine.clone();
        response[at].fill(value);
        remac(&mut response, &peers.client.public);
        response
    };

    let failures = [
        (
            genuine[..genuine.len() - 1].to_vec(),
            SessionError::Malformed,
        ),
        (changed(8..12, 5), SessionError::WrongIndex),
        (junk, SessionError::BadMac1),
        (
            changed(12..44, 0),
            SessionError::Handshake("weak ephemeral key"),
        ),
        (
            junk_past_mac1,
            SessionError::Handshake("could not decrypt response"),
        ),
        (
            changed(12..13, genuine[12] ^ 1),
            SessionError::Handshake("could not decrypt response"),
        ),
        (
            changed(59..60, genuine[59] ^ 1),
            SessionError::Handshake("could not decrypt response"),
        ),
    ];
    for (bad, expected) in &failures {
        assert_eq!(initiation.finish(bad, now).map(|_| ()), Err(*expected));
    }

    let mut client = initiation
        .finish(&genuine, now)
        .expect("the real response still finishes");
    let mut wire = Vec::new();
    let mut plain = Vec::new();
    client.encrypt(b"made it", &mut wire).expect("encrypt");
    host.decrypt(&wire, &mut plain).expect("host decrypts");
    assert_eq!(plain, b"made it");

    assert_eq!(
        initiation.finish(&genuine, now).map(|_| ()),
        Err(SessionError::Handshake("handshake already finished"))
    );
}

// Pinned, not wanted. As in WireGuard, the indices sit outside Noise and mac1 is keyed on a
// public key, so whoever holds that key can rewrite an index. A rewritten copy that beats the real
// packet stalls that one handshake attempt; the retry uses a fresh index and timestamp.
#[test]
fn sender_indices_are_not_authenticated() {
    let peers = peers();
    let now = Instant::now();

    let (mut initiation, mut packet) = start(&peers, &PEER_PSK, InitKind::Known, 1);
    packet[4..8].copy_from_slice(&0x5555u32.to_le_bytes());
    remac(&mut packet, &peers.host.public);
    let incoming = read_initiation(&peers.host.private, &peers.host.public, &packet)
        .expect("a rewritten initiation index still reads");
    assert_eq!(incoming.sender_index, 0x5555);
    let (_host, response) = incoming.accept(&PEER_PSK, 2, now).expect("accept");
    assert_eq!(
        initiation.finish(&response, now).map(|_| ()),
        Err(SessionError::WrongIndex)
    );

    let (mut initiation, packet) = start(&peers, &PEER_PSK, InitKind::Known, 1);
    let (mut host, mut response) = respond(&peers, &packet, 2, now);
    response[4..8].copy_from_slice(&0x6666u32.to_le_bytes());
    remac(&mut response, &peers.client.public);
    let mut client = initiation
        .finish(&response, now)
        .expect("a rewritten response index still finishes");
    assert_eq!(client.remote_index(), 0x6666);
    let mut wire = Vec::new();
    let mut plain = Vec::new();
    client.encrypt(b"lost", &mut wire).expect("encrypt");
    assert_eq!(
        host.decrypt(&wire, &mut plain),
        Err(SessionError::WrongIndex)
    );
    assert!(!host.is_confirmed());
}

#[test]
fn wrong_receiver_index() {
    let peers = peers();
    let now = Instant::now();
    let (mut initiation, packet) = start(&peers, &PEER_PSK, InitKind::Known, 1);
    let (_host, mut response) = respond(&peers, &packet, 2, now);
    response[8..12].copy_from_slice(&9u32.to_le_bytes());
    remac(&mut response, &peers.client.public);
    assert_eq!(response_receiver_index(&response), Some(9));
    assert_eq!(
        initiation.finish(&response, now).map(|_| ()),
        Err(SessionError::WrongIndex)
    );
}

#[test]
fn response_to_another_initiation() {
    let peers = peers();
    let now = Instant::now();
    let (mut first, _) = start(&peers, &PEER_PSK, InitKind::Known, 1);
    let (_, second_packet) = start(&peers, &PEER_PSK, InitKind::Known, 1);
    let (_host, response) = respond(&peers, &second_packet, 2, now);
    assert_eq!(
        first.finish(&response, now).map(|_| ()),
        Err(SessionError::Handshake("could not decrypt response"))
    );
}

#[test]
fn wrong_lengths_are_malformed() {
    let peers = peers();
    let (_, packet) = start(&peers, &PEER_PSK, InitKind::Known, 1);
    let read =
        |bytes: &[u8]| read_initiation(&peers.host.private, &peers.host.public, bytes).map(|_| ());
    assert_eq!(
        read(&packet[..packet.len() - 1]),
        Err(SessionError::Malformed)
    );
    let mut longer = packet.clone();
    longer.push(0);
    assert_eq!(read(&longer), Err(SessionError::Malformed));
    assert_eq!(read(&[]), Err(SessionError::Malformed));

    let mut reserved = packet.clone();
    reserved[2] = 1;
    remac(&mut reserved, &peers.host.public);
    assert_eq!(read(&reserved), Err(SessionError::Malformed));
}

#[test]
fn crafted_payloads_are_checked() {
    let peers = peers();
    let read = |bytes: &[u8]| {
        read_initiation(&peers.host.private, &peers.host.public, bytes)
            .map(|incoming| incoming.kind)
    };

    let good = craft_initiation(&peers, &PEER_PSK, PROLOGUE, &payload(1, 1, &[]), 3);
    assert_eq!(read(&good), Ok(InitKind::Known));

    let future = craft_initiation(&peers, &PEER_PSK, PROLOGUE, &payload(2, 1, &[]), 3);
    assert_eq!(
        read(&future),
        Err(SessionError::Handshake("unsupported protocol version"))
    );

    // Right lengths for the packet, but the kind byte disagrees with them.
    let invite_without_id = craft_initiation(&peers, &PEER_PSK, PROLOGUE, &payload(1, 0, &[]), 3);
    let known_with_id =
        craft_initiation(&peers, &PEER_PSK, PROLOGUE, &payload(1, 1, &INVITE_ID), 3);
    let unknown_kind = craft_initiation(&peers, &PEER_PSK, PROLOGUE, &payload(1, 7, &[]), 3);
    for bad in [invite_without_id, known_with_id, unknown_kind] {
        assert_eq!(
            read(&bad),
            Err(SessionError::Handshake("bad initiation payload"))
        );
    }

    let other_protocol = craft_initiation(&peers, &PEER_PSK, b"booth2", &payload(1, 1, &[]), 3);
    assert_eq!(
        read(&other_protocol),
        Err(SessionError::Handshake("could not decrypt initiation"))
    );
}

#[test]
fn low_order_keys_are_refused() {
    let peers = peers();
    let (_, mut packet) = start(&peers, &PEER_PSK, InitKind::Known, 1);
    packet[8..40].fill(0);
    remac(&mut packet, &peers.host.public);
    assert_eq!(
        read_initiation(&peers.host.private, &peers.host.public, &packet).map(|_| ()),
        Err(SessionError::Handshake("weak ephemeral key"))
    );

    let weak_host = [0u8; 32];
    assert_eq!(
        Initiation::start(
            &peers.client.private,
            &peers.client.public,
            &weak_host,
            &PEER_PSK,
            InitKind::Known,
            Tai64N::now(),
            1,
        )
        .map(|_| ()),
        Err(SessionError::Handshake("weak host key"))
    );

    let now = Instant::now();
    let (mut initiation, packet) = start(&peers, &PEER_PSK, InitKind::Known, 1);
    let (_host, mut response) = respond(&peers, &packet, 2, now);
    response[12..44].fill(0);
    remac(&mut response, &peers.client.public);
    assert_eq!(
        initiation.finish(&response, now).map(|_| ()),
        Err(SessionError::Handshake("weak ephemeral key"))
    );
}

#[test]
fn error_messages() {
    assert_eq!(SessionError::BadMac1.to_string(), "bad mac1");
    assert_eq!(SessionError::Replayed.to_string(), "replayed counter");
    assert_eq!(SessionError::Expired.to_string(), "session expired");
    assert_eq!(
        SessionError::Handshake("could not decrypt response").to_string(),
        "handshake failed: could not decrypt response"
    );
}

// Prints numbers, asserts nothing. Run with --nocapture, and with --release for real figures.
#[test]
fn handshake_time() {
    let peers = peers();
    let rounds: u32 = if cfg!(debug_assertions) { 20 } else { 200 };
    let mut client_side = Duration::ZERO;
    let mut host_side = Duration::ZERO;
    for index in 0..rounds {
        let now = Instant::now();
        let t0 = Instant::now();
        let (mut initiation, packet) = start(&peers, &PEER_PSK, InitKind::Known, index);
        let t1 = Instant::now();
        let incoming = read_initiation(&peers.host.private, &peers.host.public, &packet)
            .expect("read initiation");
        let (_host, response) = incoming.accept(&PEER_PSK, index, now).expect("accept");
        let t2 = Instant::now();
        let _client = initiation.finish(&response, now).expect("finish");
        let t3 = Instant::now();
        client_side += (t1 - t0) + (t3 - t2);
        host_side += t2 - t1;
    }
    let build = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    println!(
        "handshake time ({build} build, {rounds} rounds): {} us total, client {} us, host {} us",
        ((client_side + host_side) / rounds).as_micros(),
        (client_side / rounds).as_micros(),
        (host_side / rounds).as_micros(),
    );
}
