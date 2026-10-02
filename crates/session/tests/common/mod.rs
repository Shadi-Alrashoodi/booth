// Each test file uses a different part of this.
#![allow(dead_code)]

use std::time::Instant;

use blake2::digest::consts::U16;
use blake2::digest::{KeyInit, Mac};
use blake2::{Blake2s256, Blake2sMac, Digest};
use session::{
    InitKind, Initiation, NOISE_PATTERN, Session, Tai64N, TimestampSource, read_initiation,
};
use snow::Builder;

pub const INVITE_ID: [u8; 8] = *b"invite01";
pub const INVITE_SECRET: [u8; 16] = [0x42; 16];
pub const PEER_PSK: [u8; 32] = [0x17; 32];

pub struct Keys {
    pub private: [u8; 32],
    pub public: [u8; 32],
}

pub struct Peers {
    pub client: Keys,
    pub host: Keys,
}

pub fn keys() -> Keys {
    let pair = Builder::new(NOISE_PATTERN.parse().expect("pattern parses"))
        .generate_keypair()
        .expect("keypair");
    Keys {
        private: pair.private.try_into().expect("32 byte private key"),
        public: pair.public.try_into().expect("32 byte public key"),
    }
}

pub fn peers() -> Peers {
    Peers {
        client: keys(),
        host: keys(),
    }
}

pub fn start(peers: &Peers, psk: &[u8; 32], kind: InitKind, index: u32) -> (Initiation, Vec<u8>) {
    Initiation::start(
        &peers.client.private,
        &peers.client.public,
        &peers.host.public,
        psk,
        kind,
        Tai64N::now(),
        index,
    )
    .expect("start initiation")
}

// A handshake between known peers. Returns the client's session, then the host's.
pub fn handshake(peers: &Peers, now: Instant) -> (Session, Session) {
    let (mut initiation, packet) = start(peers, &PEER_PSK, InitKind::Known, 0x0102_0304);
    let (host, response) = respond(peers, &packet, 0x0a0b_0c0d, now);
    let client = initiation.finish(&response, now).expect("finish");
    (client, host)
}

// The host's side of a handshake with the known-peer psk. Returns its session and the response.
pub fn respond(peers: &Peers, initiation: &[u8], index: u32, now: Instant) -> (Session, Vec<u8>) {
    read_initiation(&peers.host.private, &peers.host.public, initiation)
        .expect("read initiation")
        .accept(&PEER_PSK, index, now)
        .expect("accept")
}

// mac1 written out again here, apart from the crate's code, for tests that craft packets.
pub fn mac1(receiver_public: &[u8; 32], covered: &[u8]) -> [u8; 16] {
    let key = Blake2s256::new()
        .chain_update(b"mac1----")
        .chain_update(receiver_public)
        .finalize();
    let mut mac = <Blake2sMac<U16> as KeyInit>::new_from_slice(&key).expect("32 byte key");
    mac.update(covered);
    mac.finalize().into_bytes().into()
}

// Replaces mac1 with a valid one for the rest of the packet and zeroes mac2.
pub fn remac(packet: &mut [u8], receiver_public: &[u8; 32]) {
    let covered_len = packet.len() - 32;
    let mac = mac1(receiver_public, &packet[..covered_len]);
    packet[covered_len..covered_len + 16].copy_from_slice(&mac);
    packet[covered_len + 16..].fill(0);
}

pub fn flipped(packet: &[u8], at: usize, bits: u8) -> Vec<u8> {
    let mut changed = packet.to_vec();
    changed[at] ^= bits;
    changed
}

// Builds an initiation straight from snow, so tests can put anything in the payload.
pub fn craft_initiation(
    peers: &Peers,
    psk: &[u8; 32],
    prologue: &[u8],
    payload: &[u8],
    index: u32,
) -> Vec<u8> {
    let mut noise = Builder::new(NOISE_PATTERN.parse().expect("pattern parses"))
        .local_private_key(&peers.client.private)
        .and_then(|b| b.remote_public_key(&peers.host.public))
        .and_then(|b| b.prologue(prologue))
        .and_then(|b| b.psk(2, psk))
        .and_then(|b| b.build_initiator())
        .expect("snow initiator");
    let mut message = vec![0u8; 256];
    let len = noise
        .write_message(payload, &mut message)
        .expect("write message 1");
    let mut packet = vec![0x11, 0, 0, 0];
    packet.extend_from_slice(&index.to_le_bytes());
    packet.extend_from_slice(&message[..len]);
    packet.extend_from_slice(&[0; 32]);
    remac(&mut packet, &peers.host.public);
    packet
}

pub fn payload(version: u8, kind: u8, extra: &[u8]) -> Vec<u8> {
    let mut payload = vec![version];
    payload.extend_from_slice(&TimestampSource::new().next_stamp().to_bytes());
    payload.push(kind);
    payload.extend_from_slice(extra);
    payload
}
