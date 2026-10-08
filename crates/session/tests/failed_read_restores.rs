mod common;

use std::time::Instant;

use common::*;
use session::{
    InitKind, Initiation, Session, SessionError, invite_psk, read_initiation,
    response_receiver_index,
};

// Initiation::finish reads a response into its snow state in place and counts on snow putting
// that state back when the read fails, so a bad response cannot spend the initiation or change
// the keys the genuine one derives later. snow 0.10 saves h, ck and has_key before a read and
// puts them back when it fails. It leaves the failed read's cipher key in place, which is harmless
// here: message 2 starts with e, and a psk pattern mixes e into the key before anything is
// decrypted. Without the put back, every test below fails when the genuine response arrives.

const UNREADABLE: SessionError = SessionError::Handshake("could not decrypt response");

// Response layout: header 0..4, sender 4..8, receiver 8..12, ephemeral 12..44, tag 44..60,
// mac1 60..76, mac2 76..92. The receiver is the initiation's sender index, which travels in the
// clear at 4..8 of the initiation, and mac1 is keyed on the client's public key, so anyone who saw
// the initiation can build a response that gets as far as snow.
fn aimed_at(initiation: &[u8], ephemeral: &[u8; 32], tag: &[u8; 16], peers: &Peers) -> Vec<u8> {
    let mut response = vec![0x12, 0, 0, 0];
    response.extend_from_slice(&9u32.to_le_bytes());
    response.extend_from_slice(&initiation[4..8]);
    response.extend_from_slice(ephemeral);
    response.extend_from_slice(tag);
    response.extend_from_slice(&[0; 32]);
    remac(&mut response, &peers.client.public);
    response
}

// The genuine response finishes the initiation, and data then flows both ways.
fn finishes_and_talks(
    initiation: &mut Initiation,
    mut host: Session,
    genuine: &[u8],
    now: Instant,
) {
    let mut client = initiation
        .finish(genuine, now)
        .expect("the genuine response still finishes");
    assert_eq!(client.remote_index(), host.local_index());
    assert_eq!(host.remote_index(), client.local_index());

    let mut wire = Vec::new();
    let mut plain = Vec::new();
    client
        .encrypt(b"client to host", &mut wire)
        .expect("client encrypts");
    host.decrypt(&wire, &mut plain).expect("host decrypts");
    assert_eq!(plain, b"client to host");
    host.encrypt(b"host to client", &mut wire)
        .expect("host encrypts");
    client.decrypt(&wire, &mut plain).expect("client decrypts");
    assert_eq!(plain, b"host to client");
}

// Junk with the right receiver index. With a bad mac1 it stops before snow. With a good one it
// goes through e, ee, se and psk before the tag fails, and the initiation is left as it was. The
// last one carries the genuine ephemeral, so snow does the same key math as for the real thing.
#[test]
fn junk_with_the_right_index_leaves_the_initiation_usable() {
    let peers = peers();
    let now = Instant::now();
    let (mut initiation, packet) = start(&peers, &PEER_PSK, InitKind::Known, 0x0102_0304);
    let (host, genuine) = respond(&peers, &packet, 0x0a0b_0c0d, now);

    let on_the_curve = keys().public;
    let genuine_ephemeral = genuine[12..44].try_into().expect("32 bytes");
    let junk = [
        aimed_at(&packet, &[0xa5; 32], &[0x5a; 16], &peers),
        aimed_at(&packet, &on_the_curve, &[0x5a; 16], &peers),
        aimed_at(&packet, genuine_ephemeral, &[0; 16], &peers),
    ];
    for bad in &junk {
        assert_eq!(response_receiver_index(bad), Some(0x0102_0304));
        assert_eq!(
            initiation.finish(&flipped(bad, 60, 0x01), now).map(|_| ()),
            Err(SessionError::BadMac1)
        );
        assert_eq!(initiation.finish(bad, now).map(|_| ()), Err(UNREADABLE));
    }

    finishes_and_talks(&mut initiation, host, &genuine, now);
}

// A response a real host made for this very initiation, with a fresh ephemeral each time but the
// wrong psk: one unrelated, one a bit off, and the invite psk where the peer psk belongs. snow
// reads e, ee, se and psk into its state before the tag fails.
#[test]
fn wrong_psk_responses_leave_the_initiation_usable() {
    let peers = peers();
    let now = Instant::now();
    let (mut initiation, packet) = start(&peers, &PEER_PSK, InitKind::Known, 0x0102_0304);

    let mut a_bit_off = PEER_PSK;
    a_bit_off[31] ^= 0x01;
    let invite = *invite_psk(&INVITE_SECRET, &peers.host.public, &peers.client.public);
    for (wrong, index) in [[0x99; 32], a_bit_off, invite].iter().zip(50..) {
        let incoming =
            read_initiation(&peers.host.private, &peers.host.public, &packet).expect("read");
        let (_host, response) = incoming
            .accept(wrong, index, now)
            .expect("accept with a wrong psk");
        assert_eq!(response_receiver_index(&response), Some(0x0102_0304));
        assert_eq!(
            initiation.finish(&response, now).map(|_| ()),
            Err(UNREADABLE)
        );
    }

    let (host, genuine) = respond(&peers, &packet, 0x0a0b_0c0d, now);
    finishes_and_talks(&mut initiation, host, &genuine, now);
}

// A response to an older initiation from the same client. As sent it names the older index and
// stops at the index check. Pointed at the current index, which anyone can do since mac1 is keyed
// on the client's public key, it is a real response that goes all the way through snow and fails
// at the tag.
#[test]
fn a_response_to_an_older_initiation_leaves_the_current_one_usable() {
    let peers = peers();
    let now = Instant::now();
    let (mut older, older_packet) = start(&peers, &PEER_PSK, InitKind::Known, 6);
    let (mut initiation, packet) = start(&peers, &PEER_PSK, InitKind::Known, 7);
    let (_older_host, stale) = respond(&peers, &older_packet, 2, now);

    assert_eq!(
        initiation.finish(&stale, now).map(|_| ()),
        Err(SessionError::WrongIndex)
    );
    let mut retargeted = stale.clone();
    retargeted[8..12].copy_from_slice(&packet[4..8]);
    remac(&mut retargeted, &peers.client.public);
    for _ in 0..3 {
        assert_eq!(
            initiation.finish(&retargeted, now).map(|_| ()),
            Err(UNREADABLE)
        );
    }
    older
        .finish(&stale, now)
        .expect("the stale response finishes the initiation it answers");

    let (host, genuine) = respond(&peers, &packet, 3, now);
    finishes_and_talks(&mut initiation, host, &genuine, now);
}

// All three kinds, one after another and over again on one initiation, so each failed read
// follows another failed read. The genuine response still finishes it.
#[test]
fn failed_reads_in_a_row_leave_the_initiation_usable() {
    let peers = peers();
    let now = Instant::now();
    let (_older, older_packet) = start(&peers, &PEER_PSK, InitKind::Known, 6);
    let (mut initiation, packet) = start(&peers, &PEER_PSK, InitKind::Known, 7);
    let (host, genuine) = respond(&peers, &packet, 3, now);

    let junk = aimed_at(&packet, &keys().public, &[0x5a; 16], &peers);
    let incoming = read_initiation(&peers.host.private, &peers.host.public, &packet).expect("read");
    let (_wrong, wrong_psk) = incoming.accept(&[0x99; 32], 4, now).expect("accept");
    let (_older_host, mut stale) = respond(&peers, &older_packet, 2, now);
    stale[8..12].copy_from_slice(&packet[4..8]);
    remac(&mut stale, &peers.client.public);

    for _ in 0..3 {
        for bad in [&junk, &wrong_psk, &stale] {
            assert_eq!(initiation.finish(bad, now).map(|_| ()), Err(UNREADABLE));
        }
    }

    finishes_and_talks(&mut initiation, host, &genuine, now);
}
