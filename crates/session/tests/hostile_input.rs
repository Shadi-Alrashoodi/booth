mod common;

use std::cell::RefCell;
use std::net::SocketAddr;
use std::ops::Range;
use std::time::Instant;

use common::*;
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::{Config, TestRunner};
use session::{
    CookieChecker, InitKind, Initiation, PROLOGUE, SessionError, Tai64N,
    cookie_reply_receiver_index, data_receiver_index, packet_type, read_cookie_reply,
    read_initiation, response_receiver_index,
};

// Also where a cookie reply keeps its receiver index.
const SENDER_INDEX: Range<usize> = 4..8;

fn runner(cases: u32) -> TestRunner {
    TestRunner::new(Config {
        cases,
        failure_persistence: None,
        ..Config::default()
    })
}

// Random bytes with our type byte and zero reserved bytes, so they get past the first check.
fn framed(kind: u8, lengths: impl Strategy<Value = usize>) -> impl Strategy<Value = Vec<u8>> {
    lengths
        .prop_flat_map(|len| vec(any::<u8>(), len))
        .prop_map(move |mut bytes| {
            for (at, value) in [kind, 0, 0, 0].into_iter().enumerate() {
                if let Some(slot) = bytes.get_mut(at) {
                    *slot = value;
                }
            }
            bytes
        })
}

fn changes() -> impl Strategy<Value = Vec<(Index, u8)>> {
    vec((any::<Index>(), 1u8..=255), 1..4)
}

fn apply(valid: &[u8], changes: &[(Index, u8)]) -> Vec<u8> {
    let mut bytes = valid.to_vec();
    for (at, bits) in changes {
        let at = at.index(bytes.len());
        bytes[at] ^= bits;
    }
    bytes
}

fn mac1_is_valid(packet: &[u8], receiver_public: &[u8; 32]) -> bool {
    let covered = packet.len() - 32;
    packet[covered..covered + 16] == mac1(receiver_public, &packet[..covered])
}

// The model for a changed handshake packet. The only change that can get through is a new sender
// index under a recomputed mac1 (see sender_indices_are_not_authenticated), and mac2 is not read.
fn should_accept(original: &[u8], changed: &[u8], receiver_public: &[u8; 32]) -> bool {
    let covered = original.len() - 32;
    original.len() == changed.len()
        && original
            .iter()
            .zip(changed)
            .enumerate()
            .all(|(at, (a, b))| a == b || SENDER_INDEX.contains(&at) || at >= covered)
        && mac1_is_valid(changed, receiver_public)
}

fn sender_index(packet: &[u8]) -> u32 {
    u32::from_le_bytes(packet[SENDER_INDEX].try_into().expect("4 bytes"))
}

#[test]
fn classifiers_never_panic() {
    runner(20_000)
        .run(&vec(any::<u8>(), 0..200), |bytes| {
            let kind = packet_type(&bytes);
            prop_assert_eq!(kind.is_some(), matches!(bytes.first(), Some(0x11..=0x15)));
            let _ = data_receiver_index(&bytes);
            let _ = response_receiver_index(&bytes);
            let _ = cookie_reply_receiver_index(&bytes);
            Ok(())
        })
        .expect("classifiers handle every input");
}

#[derive(Debug, Clone)]
enum HostileInitiation {
    // None of these can be read: random, or random behind a valid mac1.
    Junk(Vec<u8>),
    Changed {
        invite: bool,
        changes: Vec<(Index, u8)>,
        remac: bool,
    },
    // A valid Noise message 1 around a payload of one of the two lengths the packet allows, so
    // whatever is in it reaches the payload parser.
    Payload(Vec<u8>),
}

// Biased toward the version and kind bytes that pass, so every branch of the layout is reached.
fn payload() -> impl Strategy<Value = Vec<u8>> {
    (
        prop_oneof![Just(1u8), any::<u8>()],
        vec(any::<u8>(), 12),
        prop_oneof![Just(0u8), Just(1u8), any::<u8>()],
        prop_oneof![Just(Vec::new()), vec(any::<u8>(), 8)],
    )
        .prop_map(|(version, stamp, kind, invite_id)| {
            let mut payload = vec![version];
            payload.extend_from_slice(&stamp);
            payload.push(kind);
            payload.extend_from_slice(&invite_id);
            payload
        })
}

fn payload_model(payload: &[u8]) -> Result<(InitKind, Tai64N), SessionError> {
    let stamp = Tai64N::from_bytes(payload[1..13].try_into().expect("12 bytes"));
    match (payload[0], payload[13], &payload[14..]) {
        (1, 0, id) if id.len() == 8 => {
            Ok((InitKind::Invite(id.try_into().expect("8 bytes")), stamp))
        }
        (1, 1, []) => Ok((InitKind::Known, stamp)),
        (1, 2, []) => Ok((InitKind::Rekey, stamp)),
        (1, _, _) => Err(SessionError::Handshake("bad initiation payload")),
        _ => Err(SessionError::Handshake("unsupported protocol version")),
    }
}

#[test]
fn hostile_initiations() {
    let peers = peers();
    let (_, known) = start(&peers, &PEER_PSK, InitKind::Known, 1);
    let (_, invite) = start(&peers, &PEER_PSK, InitKind::Invite(INVITE_ID), 1);
    let host_public = peers.host.public;
    // Cases that reach Noise cost several scalar multiplications each, slow in a debug build, so
    // they are fewer. decode_payload has its own cheap property test in handshake.rs.
    let inputs = prop_oneof![
        4 => vec(any::<u8>(), 0..400).prop_map(HostileInitiation::Junk),
        4 => framed(0x11, 140usize..170).prop_map(HostileInitiation::Junk),
        1 => framed(0x11, prop_oneof![Just(150usize), Just(158)]).prop_map(move |mut bytes| {
            remac(&mut bytes, &host_public);
            HostileInitiation::Junk(bytes)
        }),
        4 => (any::<bool>(), changes(), any::<bool>()).prop_map(|(invite, changes, remac)| {
            HostileInitiation::Changed {
                invite,
                changes,
                remac,
            }
        }),
        2 => payload().prop_map(HostileInitiation::Payload),
    ];
    let read = |bytes: &[u8]| read_initiation(&peers.host.private, &peers.host.public, bytes);
    runner(1500)
        .run(&inputs, |input| {
            match input {
                HostileInitiation::Junk(bytes) => prop_assert!(read(&bytes).is_err()),
                HostileInitiation::Changed {
                    invite: use_invite,
                    changes,
                    remac: fix_mac,
                } => {
                    let original = if use_invite { &invite } else { &known };
                    let mut bytes = apply(original, &changes);
                    if fix_mac {
                        remac(&mut bytes, &host_public);
                    }
                    let expected = should_accept(original, &bytes, &host_public);
                    let result = read(&bytes);
                    prop_assert_eq!(result.is_ok(), expected, "{:?}", result);
                    if let Ok(incoming) = result {
                        prop_assert_eq!(incoming.sender_index, sender_index(&bytes));
                        prop_assert_eq!(incoming.remote_public, peers.client.public);
                    }
                }
                HostileInitiation::Payload(payload) => {
                    let packet = craft_initiation(&peers, &PEER_PSK, PROLOGUE, &payload, 3);
                    let result = read(&packet).map(|incoming| (incoming.kind, incoming.timestamp));
                    prop_assert_eq!(result, payload_model(&payload));
                }
            }
            Ok(())
        })
        .expect("read_initiation handles every input");
}

#[derive(Debug, Clone)]
enum HostileResponse {
    Bytes(Vec<u8>),
    // Right type, right receiver index and a valid mac1, so the bytes reach Noise.
    PassesMac1(Vec<u8>),
    Changed {
        changes: Vec<(Index, u8)>,
        remac: bool,
    },
}

#[test]
fn hostile_responses() {
    let peers = peers();
    let now = Instant::now();
    let client_public = peers.client.public;
    let fresh = || {
        let (initiation, packet) = start(&peers, &PEER_PSK, InitKind::Known, 1);
        let (_host, response) = respond(&peers, &packet, 2, now);
        (initiation, response)
    };
    // Every case goes to the same initiation, because a failed response must not spend it. It is
    // replaced only after a case the model says is acceptable.
    let current = RefCell::new(fresh());
    let inputs = prop_oneof![
        vec(any::<u8>(), 0..200).prop_map(HostileResponse::Bytes),
        framed(0x12, 80usize..100).prop_map(HostileResponse::Bytes),
        framed(0x12, Just(92usize)).prop_map(HostileResponse::PassesMac1),
        (changes(), any::<bool>())
            .prop_map(|(changes, remac)| HostileResponse::Changed { changes, remac }),
    ];
    runner(1500)
        .run(&inputs, |input| {
            let mut current = current.borrow_mut();
            let (initiation, genuine) = &mut *current;
            let response = match input {
                HostileResponse::Bytes(bytes) => bytes,
                HostileResponse::PassesMac1(mut bytes) => {
                    bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
                    remac(&mut bytes, &client_public);
                    bytes
                }
                HostileResponse::Changed {
                    changes,
                    remac: fix_mac,
                } => {
                    let mut bytes = apply(genuine, &changes);
                    if fix_mac {
                        remac(&mut bytes, &client_public);
                    }
                    bytes
                }
            };
            let expected = should_accept(genuine, &response, &client_public);
            let result = initiation.finish(&response, now);
            prop_assert_eq!(result.is_ok(), expected, "{:?}", result);
            if let Ok(session) = result {
                prop_assert_eq!(session.remote_index(), sender_index(&response));
                *current = fresh();
            }
            Ok(())
        })
        .expect("finish handles every input");

    let (mut initiation, genuine) = current.into_inner();
    initiation
        .finish(&genuine, now)
        .expect("after all that, the real response still finishes");
}

#[test]
fn decrypt_never_panics() {
    let peers = peers();
    let (mut client, host) = handshake(&peers, Instant::now());
    let mut valid = Vec::new();
    client
        .encrypt(b"a real packet", &mut valid)
        .expect("encrypt");
    let index = host.local_index();
    let host = RefCell::new(host);
    let inputs = prop_oneof![
        vec(any::<u8>(), 0..2000),
        framed(0x15, 0usize..200).prop_map(move |mut bytes| {
            if let Some(slot) = bytes.get_mut(4..8) {
                slot.copy_from_slice(&index.to_le_bytes());
            }
            bytes
        }),
        changes().prop_map({
            let valid = valid.clone();
            move |changes| apply(&valid, &changes)
        }),
    ];
    runner(5000)
        .run(&inputs, |bytes| {
            let mut plain = Vec::new();
            let result = host.borrow_mut().decrypt(&bytes, &mut plain);
            prop_assert!(result.is_err() || bytes == valid);
            prop_assert!(result.is_ok() || plain.is_empty());
            Ok(())
        })
        .expect("decrypt handles every input");
}

#[derive(Debug, Clone)]
enum HostileReply {
    Bytes(Vec<u8>),
    Changed(Vec<(Index, u8)>),
}

#[test]
fn hostile_cookie_replies() {
    let peers = peers();
    let now = Instant::now();
    let source: SocketAddr = "203.0.113.7:50123".parse().expect("address");
    let mut checker = CookieChecker::new(&peers.host.public, now);
    let (initiation, packet) = start(&peers, &PEER_PSK, InitKind::Known, 1);
    let genuine = checker
        .cookie_reply(&packet, source, now)
        .expect("cookie reply");
    let cookie = checker.cookie_for(source, now);
    let mac1 = initiation.mac1();
    let inputs = prop_oneof![
        vec(any::<u8>(), 0..200).prop_map(HostileReply::Bytes),
        framed(0x13, 56usize..72).prop_map(HostileReply::Bytes),
        framed(0x13, Just(64usize)).prop_map(HostileReply::Bytes),
        changes().prop_map(HostileReply::Changed),
    ];
    runner(5000)
        .run(&inputs, |input| {
            let bytes = match input {
                HostileReply::Bytes(bytes) => bytes,
                HostileReply::Changed(changes) => apply(&genuine, &changes),
            };
            let index = cookie_reply_receiver_index(&bytes);
            prop_assert_eq!(
                index.is_some(),
                bytes.len() == 64 && bytes[..4] == [0x13, 0, 0, 0]
            );
            // The receiver index is the one part outside the encryption.
            let expected = bytes.len() == genuine.len()
                && bytes
                    .iter()
                    .zip(&genuine)
                    .enumerate()
                    .all(|(at, (a, b))| a == b || SENDER_INDEX.contains(&at));
            let result = read_cookie_reply(&bytes, &peers.host.public, &mac1);
            prop_assert_eq!(result.is_ok(), expected, "{:?}", result);
            if let Ok(opened) = result {
                prop_assert_eq!(Some(opened), index.map(|index| (index, cookie)));
            }
            Ok(())
        })
        .expect("read_cookie_reply handles every input");
}

#[derive(Debug, Clone)]
enum HostileMac2 {
    Bytes(Vec<u8>),
    PassesMac1(Vec<u8>),
    Changed(Vec<(Index, u8)>),
}

#[test]
fn hostile_mac2() {
    let peers = peers();
    let now = Instant::now();
    let source: SocketAddr = "203.0.113.7:50123".parse().expect("address");
    let host_public = peers.host.public;
    let mut checker = CookieChecker::new(&host_public, now);
    let cookie = checker.cookie_for(source, now);
    let (_, genuine) = Initiation::start_with_cookie(
        &peers.client.private,
        &peers.client.public,
        &host_public,
        &PEER_PSK,
        InitKind::Known,
        Tai64N::now(),
        1,
        Some(&cookie),
    )
    .expect("start initiation");
    let checker = RefCell::new(checker);
    let inputs = prop_oneof![
        vec(any::<u8>(), 0..200).prop_map(HostileMac2::Bytes),
        framed(0x11, 140usize..170).prop_map(HostileMac2::Bytes),
        framed(0x11, prop_oneof![Just(150usize), Just(158)]).prop_map(move |mut bytes| {
            remac(&mut bytes, &host_public);
            HostileMac2::PassesMac1(bytes)
        }),
        changes().prop_map(HostileMac2::Changed),
    ];
    runner(5000)
        .run(&inputs, |input| {
            let mut checker = checker.borrow_mut();
            let (bytes, passes_mac1) = match input {
                HostileMac2::Bytes(bytes) => (bytes, false),
                HostileMac2::PassesMac1(bytes) => (bytes, true),
                HostileMac2::Changed(changes) => {
                    let bytes = apply(&genuine, &changes);
                    let covered = genuine.len() - 16;
                    let passes = bytes[..covered] == genuine[..covered];
                    (bytes, passes)
                }
            };
            let shaped = matches!(bytes.len(), 150 | 158) && bytes[..4] == [0x11, 0, 0, 0];
            prop_assert_eq!(checker.has_valid_mac1(&bytes), passes_mac1);
            prop_assert_eq!(
                checker.has_valid_mac2(&bytes, source, now),
                bytes == genuine
            );

            match checker.cookie_reply(&bytes, source, now) {
                Ok(reply) => {
                    prop_assert!(passes_mac1);
                    let covered = bytes.len() - 32;
                    let mac1: [u8; 16] = bytes[covered..covered + 16].try_into().expect("16");
                    prop_assert_eq!(
                        read_cookie_reply(&reply, &host_public, &mac1),
                        Ok((sender_index(&bytes), cookie))
                    );
                }
                Err(err) if shaped => {
                    prop_assert!(!passes_mac1);
                    prop_assert_eq!(err, SessionError::BadMac1);
                }
                Err(err) => prop_assert_eq!(err, SessionError::Malformed),
            }
            Ok(())
        })
        .expect("the cookie checks handle every input");
}
