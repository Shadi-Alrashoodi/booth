use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use chacha20poly1305::{AeadInPlace, KeyInit, XChaCha20Poly1305};
use snow::resolvers::{CryptoResolver, DefaultResolver};
use zeroize::Zeroizing;

use crate::SessionError;
use crate::mac::{self, cookie_key, mac1_matches, mac2_matches};
use crate::packet::{self, COOKIE_REPLY_LEN, XNONCE_LEN};

pub const COOKIE_SECRET_LIFETIME: Duration = Duration::from_secs(120);

// The host's side of WireGuard's cookie. Only the current secret is kept: a cookie is good for at
// most COOKIE_SECRET_LIFETIME and often less, and a client whose cookie went stale while the host
// is still under load is sent a new one.
pub struct CookieChecker {
    host_public: [u8; 32],
    reply_cipher: XChaCha20Poly1305,
    secret: Zeroizing<[u8; 32]>,
    secret_made: Instant,
}

impl CookieChecker {
    pub fn new(host_public: &[u8; 32], now: Instant) -> CookieChecker {
        CookieChecker {
            host_public: *host_public,
            reply_cipher: XChaCha20Poly1305::new(&cookie_key(host_public).into()),
            secret: fresh_secret(),
            secret_made: now,
        }
    }

    // One hash and no key math, so the host can count initiations that reach it and decide
    // whether it is under load before it spends anything on them.
    pub fn has_valid_mac1(&self, packet: &[u8]) -> bool {
        packet::parse_initiation(packet).is_some_and(|initiation| {
            mac1_matches(&self.host_public, initiation.covered, initiation.mac1)
        })
    }

    pub fn has_valid_mac2(&mut self, packet: &[u8], source: SocketAddr, now: Instant) -> bool {
        let Some(initiation) = packet::parse_initiation(packet) else {
            return false;
        };
        let cookie = self.cookie_for(source, now);
        mac2_matches(&cookie, initiation.mac2_covered, initiation.mac2)
    }

    pub fn cookie_for(&mut self, source: SocketAddr, now: Instant) -> [u8; 16] {
        if now.saturating_duration_since(self.secret_made) >= COOKIE_SECRET_LIFETIME {
            self.secret = fresh_secret();
            self.secret_made = now;
        }
        // A dual-stack socket reports IPv4 peers as mapped IPv6 addresses. Either spelling of
        // one address gets the same cookie.
        let port = source.port().to_be_bytes();
        let secret = self.secret.as_slice();
        match source.ip().to_canonical() {
            IpAddr::V4(ip) => mac::keyed_mac(secret, &[&ip.octets(), &port]),
            IpAddr::V6(ip) => mac::keyed_mac(secret, &[&ip.octets(), &port]),
        }
    }

    // One hash and one small encryption, and the reply is shorter than the initiation, so
    // answering a flood with these costs no key math and amplifies nothing.
    pub fn cookie_reply(
        &mut self,
        initiation: &[u8],
        source: SocketAddr,
        now: Instant,
    ) -> Result<[u8; COOKIE_REPLY_LEN], SessionError> {
        let parsed = packet::parse_initiation(initiation).ok_or(SessionError::Malformed)?;
        // The caller has checked this already, but a reply to a packet without a valid mac1
        // would tell a scanner that the port is alive, and the check is cheap.
        if !mac1_matches(&self.host_public, parsed.covered, parsed.mac1) {
            return Err(SessionError::BadMac1);
        }
        let mut sealed = self.cookie_for(source, now);
        let mut nonce = [0u8; XNONCE_LEN];
        fill_random(&mut nonce);
        let tag = self
            .reply_cipher
            .encrypt_in_place_detached(&nonce.into(), parsed.mac1, &mut sealed)
            .map_err(|_| SessionError::Encrypt)?;
        Ok(packet::cookie_reply_packet(
            parsed.sender_index,
            &nonce,
            &sealed,
            &tag.into(),
        ))
    }
}

impl fmt::Debug for CookieChecker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CookieChecker")
            .field("secret_made", &self.secret_made)
            .finish_non_exhaustive()
    }
}

// The receiver index is not covered by the encryption, as in WireGuard. The caller finds the
// initiation by that index and passes its mac1, so a changed index finds the wrong mac1 and the
// reply does not open.
pub fn read_cookie_reply(
    reply: &[u8],
    host_public: &[u8; 32],
    initiation_mac1: &[u8; 16],
) -> Result<(u32, [u8; 16]), SessionError> {
    let parsed = packet::parse_cookie_reply(reply).ok_or(SessionError::Malformed)?;
    let cipher = XChaCha20Poly1305::new(&cookie_key(host_public).into());
    let mut cookie = *parsed.sealed_cookie;
    cipher
        .decrypt_in_place_detached(
            &(*parsed.nonce).into(),
            initiation_mac1,
            &mut cookie,
            &(*parsed.tag).into(),
        )
        .map_err(|_| SessionError::Decrypt)?;
    Ok((parsed.receiver_index, cookie))
}

fn fresh_secret() -> Zeroizing<[u8; 32]> {
    let mut secret = Zeroizing::new([0u8; 32]);
    fill_random(secret.as_mut_slice());
    secret
}

// snow hands out getrandom, whose Windows 10+ backend is ProcessPrng, which cannot fail.
fn fill_random(bytes: &mut [u8]) {
    DefaultResolver
        .resolve_rng()
        .expect("snow is built with use-getrandom")
        .try_fill_bytes(bytes)
        .expect("the Windows random number generator failed");
}
