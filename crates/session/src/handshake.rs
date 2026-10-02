use std::time::Instant;

use blake2::Blake2sMac;
use blake2::digest::consts::U32;
use blake2::digest::generic_array::GenericArray;
use blake2::digest::{FixedOutput, KeyInit, Mac};
use snow::HandshakeState;
use zeroize::Zeroizing;

use crate::mac::mac1_matches;
use crate::noise::{self, Handshake, PSK_SLOT, is_weak_key};
use crate::packet::{self, INITIATION_NOISE_OVERHEAD, PAYLOAD_LEN_INVITE, RESPONSE_NOISE_LEN};
use crate::{Session, SessionError, Tai64N};

const PAYLOAD_VERSION: u8 = 1;
const KIND_INVITE: u8 = 0;
const KIND_KNOWN: u8 = 1;
const KIND_REKEY: u8 = 2;
const INVITE_PSK_LABEL: &[u8] = b"booth invite psk";

const BAD_PAYLOAD: SessionError = SessionError::Handshake("bad initiation payload");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InitKind {
    Invite([u8; 8]),
    Known,
    // Same psk as Known. The host keeps the peer's reliable channels across a rekey and
    // resets them on Invite or Known, so both sides agree on where sequence numbers start.
    Rekey,
}

pub fn invite_psk(
    secret: &[u8; 16],
    host_public: &[u8; 32],
    client_public: &[u8; 32],
) -> Zeroizing<[u8; 32]> {
    let mac = <Blake2sMac<U32> as KeyInit>::new_from_slice(secret)
        .expect("blake2s takes keys up to 32 bytes");
    let mac = mac
        .chain_update(INVITE_PSK_LABEL)
        .chain_update(host_public)
        .chain_update(client_public);
    let mut psk = Zeroizing::new([0u8; 32]);
    FixedOutput::finalize_into(mac, GenericArray::from_mut_slice(psk.as_mut_slice()));
    psk
}

#[derive(Debug)]
pub struct Initiation {
    noise: Handshake,
    sender_index: u32,
    local_public: [u8; 32],
    host_public: [u8; 32],
    mac1: [u8; 16],
}

impl Initiation {
    pub fn start(
        local_private: &[u8; 32],
        local_public: &[u8; 32],
        host_public: &[u8; 32],
        psk: &[u8; 32],
        kind: InitKind,
        timestamp: Tai64N,
        sender_index: u32,
    ) -> Result<(Initiation, Vec<u8>), SessionError> {
        Initiation::start_with_cookie(
            local_private,
            local_public,
            host_public,
            psk,
            kind,
            timestamp,
            sender_index,
            None,
        )
    }

    // The cookie is the one the host last sent to this address. With it the packet carries mac2,
    // which a host under load wants before it does any key math.
    #[expect(
        clippy::too_many_arguments,
        reason = "the arguments of start plus the cookie; a struct would only move the list"
    )]
    pub fn start_with_cookie(
        local_private: &[u8; 32],
        local_public: &[u8; 32],
        host_public: &[u8; 32],
        psk: &[u8; 32],
        kind: InitKind,
        timestamp: Tai64N,
        sender_index: u32,
        cookie: Option<&[u8; 16]>,
    ) -> Result<(Initiation, Vec<u8>), SessionError> {
        if is_weak_key(host_public) {
            return Err(SessionError::Handshake("weak host key"));
        }
        let mut noise = noise::initiator(local_private, host_public, psk)?;
        let payload = encode_payload(kind, timestamp);
        let mut message = [0u8; INITIATION_NOISE_OVERHEAD + PAYLOAD_LEN_INVITE];
        let written = noise
            .state()?
            .write_message(&payload, &mut message)
            .map_err(|_| SessionError::Handshake("could not write initiation"))?;
        let message = message
            .get(..written)
            .ok_or(SessionError::Handshake("could not write initiation"))?;
        let (packet, mac1) = packet::initiation_packet(sender_index, message, host_public, cookie);
        let initiation = Initiation {
            noise,
            sender_index,
            local_public: *local_public,
            host_public: *host_public,
            mac1,
        };
        Ok((initiation, packet))
    }

    pub fn sender_index(&self) -> u32 {
        self.sender_index
    }

    // A cookie reply is sealed against this, so one made by someone who never saw the initiation
    // does not open.
    pub fn mac1(&self) -> [u8; 16] {
        self.mac1
    }

    // Anyone who saw the initiation knows its index and can aim junk at it, so a response that
    // fails any check leaves the initiation as it was and the real one can still finish it. snow
    // puts its handshake state back when a read fails. Once a response succeeds it is spent.
    pub fn finish(&mut self, response: &[u8], now: Instant) -> Result<Session, SessionError> {
        let noise = self.noise.state()?;
        let response = packet::parse_response(response).ok_or(SessionError::Malformed)?;
        if response.receiver_index != self.sender_index {
            return Err(SessionError::WrongIndex);
        }
        if !mac1_matches(&self.local_public, response.covered, response.mac1) {
            return Err(SessionError::BadMac1);
        }
        if is_weak_key(response.ephemeral) {
            return Err(SessionError::Handshake("weak ephemeral key"));
        }
        noise
            .read_message(response.noise, &mut [])
            .map_err(|_| SessionError::Handshake("could not decrypt response"))?;
        let transport = self.noise.take_transport()?;
        Ok(Session::new(
            transport,
            self.sender_index,
            response.sender_index,
            self.host_public,
            now,
        ))
    }
}

// The room decides: newer timestamp for this key, live invite or known key, not blocked, which psk.
#[derive(Debug)]
pub struct IncomingInitiation {
    pub remote_public: [u8; 32],
    pub kind: InitKind,
    pub timestamp: Tai64N,
    // Outside Noise and covered only by mac1, which anyone with our public key can recompute, so
    // it says where to send the response and nothing about who sent the packet.
    pub sender_index: u32,
    noise: Handshake,
}

pub fn read_initiation(
    local_private: &[u8; 32],
    local_public: &[u8; 32],
    packet: &[u8],
) -> Result<IncomingInitiation, SessionError> {
    let initiation = packet::parse_initiation(packet).ok_or(SessionError::Malformed)?;
    if !mac1_matches(local_public, initiation.covered, initiation.mac1) {
        return Err(SessionError::BadMac1);
    }
    if is_weak_key(initiation.ephemeral) {
        return Err(SessionError::Handshake("weak ephemeral key"));
    }

    let mut noise = noise::responder(local_private)?;
    let state = noise.state()?;
    let mut payload = [0u8; PAYLOAD_LEN_INVITE];
    let read = state
        .read_message(initiation.noise, &mut payload)
        .map_err(|_| SessionError::Handshake("could not decrypt initiation"))?;
    let (kind, timestamp) = decode_payload(payload.get(..read).ok_or(BAD_PAYLOAD)?)?;

    let remote_public = remote_static(state)?;
    if is_weak_key(&remote_public) {
        return Err(SessionError::Handshake("weak static key"));
    }

    Ok(IncomingInitiation {
        remote_public,
        kind,
        timestamp,
        sender_index: initiation.sender_index,
        noise,
    })
}

impl IncomingInitiation {
    pub fn accept(
        mut self,
        psk: &[u8; 32],
        local_index: u32,
        now: Instant,
    ) -> Result<(Session, Vec<u8>), SessionError> {
        let state = self.noise.state()?;
        // From the Noise state, not the public field, which the caller can change.
        let remote_public = remote_static(state)?;
        state
            .set_psk(usize::from(PSK_SLOT), psk)
            .map_err(|_| SessionError::Handshake("could not set psk"))?;
        let mut message = [0u8; RESPONSE_NOISE_LEN];
        let written = state
            .write_message(&[], &mut message)
            .map_err(|_| SessionError::Handshake("could not write response"))?;
        if written != RESPONSE_NOISE_LEN {
            return Err(SessionError::Handshake("could not write response"));
        }
        let packet =
            packet::response_packet(local_index, self.sender_index, &message, &remote_public);
        let transport = self.noise.take_transport()?;
        let session = Session::new(
            transport,
            local_index,
            self.sender_index,
            remote_public,
            now,
        );
        Ok((session, packet))
    }
}

fn remote_static(noise: &HandshakeState) -> Result<[u8; 32], SessionError> {
    noise
        .get_remote_static()
        .and_then(|key| key.try_into().ok())
        .ok_or(SessionError::Handshake("no static key in initiation"))
}

fn encode_payload(kind: InitKind, timestamp: Tai64N) -> Vec<u8> {
    let mut payload = Vec::with_capacity(PAYLOAD_LEN_INVITE);
    payload.push(PAYLOAD_VERSION);
    payload.extend_from_slice(&timestamp.to_bytes());
    match kind {
        InitKind::Invite(invite_id) => {
            payload.push(KIND_INVITE);
            payload.extend_from_slice(&invite_id);
        }
        InitKind::Known => payload.push(KIND_KNOWN),
        InitKind::Rekey => payload.push(KIND_REKEY),
    }
    payload
}

fn decode_payload(payload: &[u8]) -> Result<(InitKind, Tai64N), SessionError> {
    let (&version, rest) = payload.split_first().ok_or(BAD_PAYLOAD)?;
    if version != PAYLOAD_VERSION {
        return Err(SessionError::Handshake("unsupported protocol version"));
    }
    let (timestamp, rest) = rest.split_first_chunk::<12>().ok_or(BAD_PAYLOAD)?;
    let kind = match rest.split_first() {
        Some((&KIND_INVITE, invite_id)) => {
            InitKind::Invite(invite_id.try_into().map_err(|_| BAD_PAYLOAD)?)
        }
        Some((&KIND_KNOWN, [])) => InitKind::Known,
        Some((&KIND_REKEY, [])) => InitKind::Rekey,
        _ => return Err(BAD_PAYLOAD),
    };
    Ok((kind, Tai64N::from_bytes(*timestamp)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::collection::vec;
    use proptest::prelude::*;

    fn model(payload: &[u8]) -> Result<(InitKind, Tai64N), SessionError> {
        match payload {
            [] => Err(BAD_PAYLOAD),
            [version, ..] if *version != PAYLOAD_VERSION => {
                Err(SessionError::Handshake("unsupported protocol version"))
            }
            [_, rest @ ..] if rest.len() < 13 => Err(BAD_PAYLOAD),
            [_, rest @ ..] => {
                let stamp = Tai64N::from_bytes(rest[..12].try_into().expect("12 bytes"));
                match (rest[12], &rest[13..]) {
                    (KIND_INVITE, id) if id.len() == 8 => {
                        Ok((InitKind::Invite(id.try_into().expect("8 bytes")), stamp))
                    }
                    (KIND_KNOWN, []) => Ok((InitKind::Known, stamp)),
                    (KIND_REKEY, []) => Ok((InitKind::Rekey, stamp)),
                    _ => Err(BAD_PAYLOAD),
                }
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(5000))]

        // Biased toward the version and kind bytes that pass, and toward the two real lengths.
        #[test]
        fn decode_payload_matches_the_model(
            version in prop_oneof![Just(PAYLOAD_VERSION), any::<u8>()],
            stamp in vec(any::<u8>(), 12),
            kind in prop_oneof![Just(KIND_INVITE), Just(KIND_KNOWN), Just(KIND_REKEY), any::<u8>()],
            extra in prop_oneof![Just(Vec::new()), vec(any::<u8>(), 8), vec(any::<u8>(), 0..12)],
            cut in prop_oneof![Just(usize::MAX), 0usize..24],
        ) {
            let mut payload = vec![version];
            payload.extend_from_slice(&stamp);
            payload.push(kind);
            payload.extend_from_slice(&extra);
            payload.truncate(cut);
            prop_assert_eq!(decode_payload(&payload), model(&payload));
        }
    }

    #[test]
    fn payload_round_trips() {
        let stamp = Tai64N::now();
        for kind in [
            InitKind::Invite([1, 2, 3, 4, 5, 6, 7, 8]),
            InitKind::Known,
            InitKind::Rekey,
        ] {
            let payload = encode_payload(kind, stamp);
            assert_eq!(decode_payload(&payload), Ok((kind, stamp)));
        }
    }

    #[test]
    fn payload_rejects_bad_shapes() {
        let stamp = Tai64N::now().to_bytes();
        let mut invite_short = vec![PAYLOAD_VERSION];
        invite_short.extend_from_slice(&stamp);
        invite_short.extend_from_slice(&[KIND_INVITE, 1, 2, 3]);
        let mut known_long = vec![PAYLOAD_VERSION];
        known_long.extend_from_slice(&stamp);
        known_long.extend_from_slice(&[KIND_KNOWN, 0]);
        let mut unknown_kind = vec![PAYLOAD_VERSION];
        unknown_kind.extend_from_slice(&stamp);
        unknown_kind.push(3);
        let mut future_version = vec![2];
        future_version.extend_from_slice(&stamp);
        future_version.push(KIND_KNOWN);

        for bad in [
            &[][..],
            &[PAYLOAD_VERSION][..],
            &invite_short,
            &known_long,
            &unknown_kind,
        ] {
            assert_eq!(decode_payload(bad), Err(BAD_PAYLOAD), "{bad:?}");
        }
        assert_eq!(
            decode_payload(&future_version),
            Err(SessionError::Handshake("unsupported protocol version"))
        );
    }

    #[test]
    fn invite_psk_inputs() {
        let secret = [3u8; 16];
        let host = [1u8; 32];
        let client = [2u8; 32];
        let base = invite_psk(&secret, &host, &client);
        assert_ne!(*base, *invite_psk(&[4u8; 16], &host, &client));
        assert_ne!(*base, *invite_psk(&secret, &client, &host));
        assert_ne!(*base, *invite_psk(&secret, &host, &[9u8; 32]));
        assert_eq!(*base, *invite_psk(&secret, &host, &client));
    }
}
