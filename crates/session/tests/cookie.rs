mod common;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use blake2::digest::consts::U16;
use blake2::digest::{KeyInit, Mac};
use blake2::{Blake2s256, Blake2sMac, Digest};
use chacha20poly1305::{AeadInPlace, XChaCha20Poly1305};
use common::*;
use session::{
    COOKIE_REPLY_LEN, COOKIE_SECRET_LIFETIME, CookieChecker, InitKind, Initiation, PacketType,
    SessionError, Tai64N, cookie_reply_receiver_index, packet_type, read_cookie_reply,
};

fn start_with(peers: &Peers, index: u32, cookie: Option<&[u8; 16]>) -> (Initiation, Vec<u8>) {
    Initiation::start_with_cookie(
        &peers.client.private,
        &peers.client.public,
        &peers.host.public,
        &PEER_PSK,
        InitKind::Known,
        Tai64N::now(),
        index,
        cookie,
    )
    .expect("start initiation")
}

fn addr(text: &str) -> SocketAddr {
    text.parse().expect("socket address")
}

fn mac2_of(packet: &[u8]) -> &[u8] {
    &packet[packet.len() - 16..]
}

// The client's side: take the cookie out of the host's reply to this initiation.
fn cookie_from(
    checker: &mut CookieChecker,
    peers: &Peers,
    source: SocketAddr,
    now: Instant,
) -> [u8; 16] {
    let (initiation, packet) = start_with(peers, 5, None);
    let reply = checker
        .cookie_reply(&packet, source, now)
        .expect("cookie reply");
    let (_, cookie) =
        read_cookie_reply(&reply, &peers.host.public, &initiation.mac1()).expect("reply opens");
    cookie
}

#[test]
fn cookie_then_mac2() {
    let peers = peers();
    let now = Instant::now();
    let source = addr("203.0.113.7:50123");
    let mut checker = CookieChecker::new(&peers.host.public, now);

    let (first, packet) = start_with(&peers, 0x1111_2222, None);
    assert!(checker.has_valid_mac1(&packet));
    assert_eq!(mac2_of(&packet), [0; 16], "no cookie, no mac2");
    assert!(!checker.has_valid_mac2(&packet, source, now));

    let reply = checker
        .cookie_reply(&packet, source, now)
        .expect("cookie reply");
    assert_eq!(packet_type(&reply), Some(PacketType::CookieReply));
    assert_eq!(cookie_reply_receiver_index(&reply), Some(0x1111_2222));
    let (index, cookie) =
        read_cookie_reply(&reply, &peers.host.public, &first.mac1()).expect("reply opens");
    assert_eq!(index, first.sender_index());
    assert_eq!(cookie, checker.cookie_for(source, now));

    let later = now + Duration::from_secs(5);
    let (mut second, packet) = start_with(&peers, 0x3333_4444, Some(&cookie));
    assert!(checker.has_valid_mac1(&packet));
    assert!(checker.has_valid_mac2(&packet, source, later));

    // mac2 changes nothing for the handshake itself.
    let (host, response) = respond(&peers, &packet, 0x5555_6666, later);
    let client = second.finish(&response, later).expect("finish");
    assert_eq!(client.remote_index(), host.local_index());
    assert_eq!(host.remote_index(), 0x3333_4444);
}

// Built byte by byte, apart from the crate's code: a change to the layout or the keys fails here.
#[test]
fn reply_and_mac2_layout() {
    let peers = peers();
    let now = Instant::now();
    let source = addr("198.51.100.20:41234");
    let mut checker = CookieChecker::new(&peers.host.public, now);

    let (initiation, packet) = start_with(&peers, 0x0a0b_0c0d, None);
    let len = packet.len();
    assert_eq!(packet[len - 32..len - 16], initiation.mac1());
    let reply = checker
        .cookie_reply(&packet, source, now)
        .expect("cookie reply");
    assert_eq!(reply[..8], [0x13, 0, 0, 0, 0x0d, 0x0c, 0x0b, 0x0a]);

    let key = Blake2s256::new()
        .chain_update(b"cookie--")
        .chain_update(peers.host.public)
        .finalize();
    let nonce: [u8; 24] = reply[8..32].try_into().expect("24 bytes");
    let mut cookie: [u8; 16] = reply[32..48].try_into().expect("16 bytes");
    let tag: [u8; 16] = reply[48..].try_into().expect("16 bytes");
    XChaCha20Poly1305::new(&key)
        .decrypt_in_place_detached(&nonce.into(), &initiation.mac1(), &mut cookie, &tag.into())
        .expect("opens with the cookie key and mac1 as additional data");
    assert_eq!(cookie, checker.cookie_for(source, now));

    let (_, packet) = start_with(&peers, 1, Some(&cookie));
    let len = packet.len();
    let mut mac = <Blake2sMac<U16> as KeyInit>::new_from_slice(&cookie).expect("16 byte key");
    mac.update(&packet[..len - 16]);
    let expected: [u8; 16] = mac.finalize().into_bytes().into();
    assert_eq!(mac2_of(&packet), expected);
}

#[test]
fn reply_opens_only_with_its_mac1() {
    let peers = peers();
    let now = Instant::now();
    let source = addr("203.0.113.7:50123");
    let mut checker = CookieChecker::new(&peers.host.public, now);
    let (first, packet) = start_with(&peers, 1, None);
    let (other, _) = start_with(&peers, 2, None);
    let reply = checker
        .cookie_reply(&packet, source, now)
        .expect("cookie reply");

    assert_eq!(
        read_cookie_reply(&reply, &peers.host.public, &other.mac1()),
        Err(SessionError::Decrypt)
    );
    assert_eq!(
        read_cookie_reply(&reply, &peers.client.public, &first.mac1()),
        Err(SessionError::Decrypt),
        "sealed under another host's key"
    );
    assert!(read_cookie_reply(&reply, &peers.host.public, &first.mac1()).is_ok());
}

// The index is outside the encryption, as in WireGuard; read_cookie_reply says why.
#[test]
fn flipped_reply_bytes() {
    let peers = peers();
    let now = Instant::now();
    let source = addr("203.0.113.7:50123");
    let mut checker = CookieChecker::new(&peers.host.public, now);
    let (initiation, packet) = start_with(&peers, 0x0102_0304, None);
    let reply = checker
        .cookie_reply(&packet, source, now)
        .expect("cookie reply");
    let cookie = checker.cookie_for(source, now);
    let mac1 = initiation.mac1();

    for at in 0..reply.len() {
        for bits in [0x01, 0x80, 0xff] {
            let changed = flipped(&reply, at, bits);
            let result = read_cookie_reply(&changed, &peers.host.public, &mac1);
            match at {
                0..4 => assert_eq!(result, Err(SessionError::Malformed), "byte {at}"),
                4..8 => {
                    let index = u32::from_le_bytes(changed[4..8].try_into().expect("4 bytes"));
                    assert_eq!(result, Ok((index, cookie)), "byte {at}");
                    assert_ne!(index, 0x0102_0304);
                }
                _ => assert_eq!(
                    result,
                    Err(SessionError::Decrypt),
                    "byte {at} xor {bits:#x}"
                ),
            }
        }
    }
    assert_eq!(
        read_cookie_reply(&reply[..63], &peers.host.public, &mac1),
        Err(SessionError::Malformed)
    );
    let mut longer = reply.to_vec();
    longer.push(0);
    assert_eq!(
        read_cookie_reply(&longer, &peers.host.public, &mac1),
        Err(SessionError::Malformed)
    );
}

#[test]
fn cookie_is_tied_to_its_address() {
    let peers = peers();
    let now = Instant::now();
    let mut checker = CookieChecker::new(&peers.host.public, now);
    let cookie = cookie_from(&mut checker, &peers, addr("203.0.113.7:50123"), now);
    let (_, packet) = start_with(&peers, 1, Some(&cookie));

    assert!(checker.has_valid_mac2(&packet, addr("203.0.113.7:50123"), now));
    assert!(
        checker.has_valid_mac2(&packet, addr("[::ffff:203.0.113.7]:50123"), now),
        "the same address as a dual-stack socket reports it"
    );
    for elsewhere in [
        "203.0.113.7:50124",
        "203.0.113.8:50123",
        "[2001:db8::7]:50123",
        "[::203.0.113.7]:50123",
    ] {
        assert!(
            !checker.has_valid_mac2(&packet, addr(elsewhere), now),
            "{elsewhere}"
        );
    }

    let v6 = addr("[2001:db8::7]:50123");
    let cookie = cookie_from(&mut checker, &peers, v6, now);
    let (_, packet) = start_with(&peers, 1, Some(&cookie));
    assert!(checker.has_valid_mac2(&packet, v6, now));
    assert!(!checker.has_valid_mac2(&packet, addr("[2001:db8::8]:50123"), now));
}

#[test]
fn mac2_covers_every_byte_before_it() {
    let peers = peers();
    let now = Instant::now();
    let source = addr("203.0.113.7:50123");
    let mut checker = CookieChecker::new(&peers.host.public, now);
    let cookie = cookie_from(&mut checker, &peers, source, now);
    let (_, packet) = start_with(&peers, 1, Some(&cookie));
    assert!(checker.has_valid_mac2(&packet, source, now));
    for at in 0..packet.len() {
        let changed = flipped(&packet, at, 0x01);
        assert!(!checker.has_valid_mac2(&changed, source, now), "byte {at}");
    }
    assert!(!checker.has_valid_mac2(&packet[..packet.len() - 1], source, now));
}

#[test]
fn secret_changes_after_its_lifetime() {
    assert_eq!(COOKIE_SECRET_LIFETIME, Duration::from_secs(120));
    let peers = peers();
    let start = Instant::now();
    let source = addr("203.0.113.7:50123");
    let mut checker = CookieChecker::new(&peers.host.public, start);
    let old = cookie_from(&mut checker, &peers, source, start);
    let (_, packet) = start_with(&peers, 1, Some(&old));

    assert!(checker.has_valid_mac2(&packet, source, start + Duration::from_secs(119)));
    let rotated = start + COOKIE_SECRET_LIFETIME;
    assert!(!checker.has_valid_mac2(&packet, source, rotated));

    let new = cookie_from(&mut checker, &peers, source, rotated);
    assert_ne!(new, old);
    let (_, packet) = start_with(&peers, 2, Some(&new));
    let later = rotated + Duration::from_secs(60);
    assert!(checker.has_valid_mac2(&packet, source, later));
    assert_eq!(checker.cookie_for(source, later), new);
}

#[test]
fn earlier_clock_keeps_the_secret() {
    let peers = peers();
    let now = Instant::now();
    let source = addr("203.0.113.7:50123");
    let mut checker = CookieChecker::new(&peers.host.public, now + Duration::from_secs(10));
    let cookie = checker.cookie_for(source, now);
    assert_eq!(
        checker.cookie_for(source, now + Duration::from_secs(10)),
        cookie
    );
}

#[test]
fn each_host_has_its_own_secret() {
    let peers = peers();
    let now = Instant::now();
    let source = addr("203.0.113.7:50123");
    let mut one = CookieChecker::new(&peers.host.public, now);
    let mut two = CookieChecker::new(&peers.host.public, now);
    assert_ne!(one.cookie_for(source, now), two.cookie_for(source, now));
}

#[test]
fn reply_is_smaller_than_any_initiation() {
    let peers = peers();
    let now = Instant::now();
    let source = addr("203.0.113.7:50123");
    let mut checker = CookieChecker::new(&peers.host.public, now);
    let (_, known) = start(&peers, &PEER_PSK, InitKind::Known, 1);
    let (_, invite) = start(&peers, &PEER_PSK, InitKind::Invite(INVITE_ID), 1);

    for initiation in [&known, &invite] {
        let reply = checker
            .cookie_reply(initiation, source, now)
            .expect("cookie reply");
        assert_eq!(reply.len(), COOKIE_REPLY_LEN);
        assert_eq!(COOKIE_REPLY_LEN, 64);
        assert!(
            reply.len() < initiation.len(),
            "the host must never send more bytes than it received before the handshake completes"
        );
    }
}

#[test]
fn reply_needs_a_valid_mac1() {
    let peers = peers();
    let now = Instant::now();
    let source = addr("203.0.113.7:50123");
    let mut checker = CookieChecker::new(&peers.host.public, now);
    let (_, packet) = start(&peers, &PEER_PSK, InitKind::Known, 1);

    let mut for_another_host = packet.clone();
    remac(&mut for_another_host, &peers.client.public);
    assert!(!checker.has_valid_mac1(&for_another_host));
    assert_eq!(
        checker.cookie_reply(&for_another_host, source, now),
        Err(SessionError::BadMac1)
    );

    let changed = flipped(&packet, 20, 0x01);
    assert!(!checker.has_valid_mac1(&changed));
    assert_eq!(
        checker.cookie_reply(&changed, source, now),
        Err(SessionError::BadMac1)
    );

    let mut response_type = packet.clone();
    response_type[0] = 0x12;
    remac(&mut response_type, &peers.host.public);
    assert!(!checker.has_valid_mac1(&response_type));
    assert_eq!(
        checker.cookie_reply(&response_type, source, now),
        Err(SessionError::Malformed)
    );
    assert_eq!(
        checker.cookie_reply(&packet[..149], source, now),
        Err(SessionError::Malformed)
    );
}
