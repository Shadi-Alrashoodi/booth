use std::hint::black_box;

use snow::params::NoiseParams;
use snow::{Builder, HandshakeState, StatelessTransportState};

use crate::wipe::WipingResolver;
use crate::{NOISE_PATTERN, PROLOGUE, SessionError};

pub(crate) const PSK_SLOT: u8 = 2;

const SETUP_FAILED: SessionError = SessionError::Handshake("could not set up noise");
const SPENT: SessionError = SessionError::Handshake("handshake already finished");

// snow keeps the psk inline in its handshake state and never clears it, so this does, on the
// way into transport mode and when a handshake is abandoned. Boxed so that moving an initiation
// in and out of the caller's map does not leave copies of the state behind.
#[derive(Debug)]
pub(crate) struct Handshake(Option<Box<HandshakeState>>);

impl Handshake {
    pub(crate) fn state(&mut self) -> Result<&mut HandshakeState, SessionError> {
        self.0.as_deref_mut().ok_or(SPENT)
    }

    pub(crate) fn take_transport(&mut self) -> Result<StatelessTransportState, SessionError> {
        let mut state = self.0.take().ok_or(SPENT)?;
        forget_psk(&mut state);
        (*state)
            .into_stateless_transport_mode()
            .map_err(|_| SessionError::Handshake("noise did not finish"))
    }
}

impl Drop for Handshake {
    fn drop(&mut self) {
        if let Some(state) = self.0.as_deref_mut() {
            forget_psk(state);
        }
    }
}

fn forget_psk(state: &mut HandshakeState) {
    // set_psk only fails for a slot out of range, and PSK_SLOT is in range.
    let _ = state.set_psk(usize::from(PSK_SLOT), &[0; 32]);
    black_box(state);
}

pub(crate) fn initiator(
    local_private: &[u8; 32],
    host_public: &[u8; 32],
    psk: &[u8; 32],
) -> Result<Handshake, SessionError> {
    builder()?
        .local_private_key(local_private)
        .and_then(|builder| builder.remote_public_key(host_public))
        .and_then(|builder| builder.prologue(PROLOGUE))
        .and_then(|builder| builder.psk(PSK_SLOT, psk))
        .and_then(|builder| builder.build_initiator())
        .map(|state| Handshake(Some(Box::new(state))))
        .map_err(|_| SETUP_FAILED)
}

// IKpsk2 mixes the psk in only at the end of message 2, so the host can read message 1 first and
// pick the psk from what it says.
pub(crate) fn responder(local_private: &[u8; 32]) -> Result<Handshake, SessionError> {
    builder()?
        .local_private_key(local_private)
        .and_then(|builder| builder.prologue(PROLOGUE))
        .and_then(|builder| builder.build_responder())
        .map(|state| Handshake(Some(Box::new(state))))
        .map_err(|_| SETUP_FAILED)
}

fn builder<'a>() -> Result<Builder<'a>, SessionError> {
    let params: NoiseParams = NOISE_PATTERN.parse().map_err(|_| SETUP_FAILED)?;
    Ok(Builder::with_resolver(params, Box::new(WipingResolver)))
}

// X25519 with any of these gives an all-zero shared secret whatever the private key is, which
// WireGuard refuses. Values from the usual small-order list, checked against snow in the tests.
const LOW_ORDER: [[u8; 32]; 7] = [
    [0; 32],
    [
        1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0,
    ],
    [
        0xe0, 0xeb, 0x7a, 0x7c, 0x3b, 0x41, 0xb8, 0xae, 0x16, 0x56, 0xe3, 0xfa, 0xf1, 0x9f, 0xc4,
        0x6a, 0xda, 0x09, 0x8d, 0xeb, 0x9c, 0x32, 0xb1, 0xfd, 0x86, 0x62, 0x05, 0x16, 0x5f, 0x49,
        0xb8, 0x00,
    ],
    [
        0x5f, 0x9c, 0x95, 0xbc, 0xa3, 0x50, 0x8c, 0x24, 0xb1, 0xd0, 0xb1, 0x55, 0x9c, 0x83, 0xef,
        0x5b, 0x04, 0x44, 0x5c, 0xc4, 0x58, 0x1c, 0x8e, 0x86, 0xd8, 0x22, 0x4e, 0xdd, 0xd0, 0x9f,
        0x11, 0x57,
    ],
    high_end(0xec),
    high_end(0xed),
    high_end(0xee),
];

// p - 1, p and p + 1, where p = 2^255 - 19.
const fn high_end(low_byte: u8) -> [u8; 32] {
    let mut key = [0xff; 32];
    key[0] = low_byte;
    key[31] = 0x7f;
    key
}

pub(crate) fn is_weak_key(key: &[u8; 32]) -> bool {
    // X25519 ignores the top bit, so a key differing only there is the same point.
    let mut masked = *key;
    masked[31] &= 0x7f;
    LOW_ORDER.contains(&masked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use snow::params::DHChoice;
    use snow::resolvers::{CryptoResolver, DefaultResolver};

    fn shared_secret(private: &[u8; 32], public: &[u8; 32]) -> [u8; 32] {
        let mut dh = DefaultResolver
            .resolve_dh(&DHChoice::Curve25519)
            .expect("default resolver has curve25519");
        dh.set(private);
        let mut out = [0u8; 32];
        dh.dh(public, &mut out).expect("dh runs");
        out
    }

    #[test]
    fn low_order_keys_give_a_zero_secret() {
        let private = [0x5a; 32];
        for key in LOW_ORDER {
            let mut with_top_bit = key;
            with_top_bit[31] |= 0x80;
            assert_eq!(shared_secret(&private, &key), [0; 32], "{key:02x?}");
            assert_eq!(
                shared_secret(&private, &with_top_bit),
                [0; 32],
                "{key:02x?}"
            );
            assert!(is_weak_key(&key));
            assert!(is_weak_key(&with_top_bit));
        }
    }

    #[test]
    fn real_keys_are_not_weak() {
        let keypair = Builder::new(NOISE_PATTERN.parse().expect("pattern parses"))
            .generate_keypair()
            .expect("keypair");
        let public: [u8; 32] = keypair.public.try_into().expect("32 byte key");
        assert!(!is_weak_key(&public));
    }
}
