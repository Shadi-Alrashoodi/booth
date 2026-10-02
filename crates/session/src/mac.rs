use blake2::digest::consts::U16;
use blake2::digest::{KeyInit, Mac};
use blake2::{Blake2s256, Blake2sMac, Digest};
use subtle::ConstantTimeEq;

const MAC1_LABEL: &[u8] = b"mac1----";
const COOKIE_LABEL: &[u8] = b"cookie--";

pub(crate) fn mac1(receiver_public: &[u8; 32], covered: &[u8]) -> [u8; 16] {
    let key = Blake2s256::new()
        .chain_update(MAC1_LABEL)
        .chain_update(receiver_public)
        .finalize();
    <Blake2sMac<U16> as KeyInit>::new(&key)
        .chain_update(covered)
        .finalize()
        .into_bytes()
        .into()
}

pub(crate) fn mac1_matches(
    receiver_public: &[u8; 32],
    covered: &[u8],
    received: &[u8; 16],
) -> bool {
    let expected = mac1(receiver_public, covered);
    expected.as_slice().ct_eq(received.as_slice()).into()
}

pub(crate) fn mac2(cookie: &[u8; 16], covered: &[u8]) -> [u8; 16] {
    keyed_mac(cookie, &[covered])
}

pub(crate) fn mac2_matches(cookie: &[u8; 16], covered: &[u8], received: &[u8; 16]) -> bool {
    let expected = mac2(cookie, covered);
    expected.as_slice().ct_eq(received.as_slice()).into()
}

// Anyone with the receiver's public key can make this key. It ties a cookie reply to one host;
// the mac1 the reply is sealed against is what ties it to one initiation.
pub(crate) fn cookie_key(receiver_public: &[u8; 32]) -> [u8; 32] {
    Blake2s256::new()
        .chain_update(COOKIE_LABEL)
        .chain_update(receiver_public)
        .finalize()
        .into()
}

pub(crate) fn keyed_mac(key: &[u8], parts: &[&[u8]]) -> [u8; 16] {
    let mut mac = <Blake2sMac<U16> as KeyInit>::new_from_slice(key)
        .expect("blake2s takes keys up to 32 bytes");
    for part in parts {
        mac.update(part);
    }
    mac.finalize().into_bytes().into()
}
