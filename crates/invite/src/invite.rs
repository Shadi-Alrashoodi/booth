use std::fmt;
use std::net::SocketAddr;

use crate::text::{self, CHECKSUM_LEN, Kind, PREAMBLE_LEN};
use crate::wire::{self, Hex, Reader};
use crate::{BuildError, Candidate, CodeError, Mapping};

pub const SINGLE_USE_SECS: u64 = 10 * 60;
pub const MULTI_USE_SECS: u64 = 24 * 60 * 60;
pub const MAX_CANDIDATES: usize = 16;

const MULTI_USE: u8 = 1 << 0;
const MAPPED: u8 = 1 << 1;
const MAPPED_VERIFIED: u8 = 1 << 2;
const SECOND_ROUTER: u8 = 1 << 3;
const MAPPING_SHIFT: u8 = 4;
const HAS_HOSTNAME: u8 = 1 << 6;
const KNOWN_FLAGS: u8 = 0x7f;

// layout, protocol and version, flags, host key, invite id, secret, expiry, candidate count,
// every candidate IPv6, hostname length and hostname, checksum
pub(crate) const MAX_BODY: usize = 1
    + PREAMBLE_LEN
    + 1
    + 32
    + 8
    + 16
    + 4
    + 1
    + MAX_CANDIDATES * (1 + wire::V6_LEN)
    + 1
    + wire::MAX_HOSTNAME
    + CHECKSUM_LEN;

#[derive(Clone, PartialEq, Eq)]
pub struct Invite {
    pub host_key: [u8; 32],
    pub invite_id: [u8; 8],
    pub secret: [u8; 16],
    pub multi_use: bool,
    pub expires_at: u64,
    pub candidates: Vec<Candidate>,
    pub mapping: Mapping,
    pub mapped: bool,
    pub mapped_verified: bool,
    pub second_router: bool,
    pub hostname: Option<String>,
}

impl Invite {
    #[allow(
        clippy::too_many_arguments,
        reason = "one argument per invite field the host decides"
    )]
    pub fn new(
        host_key: [u8; 32],
        candidates: Vec<Candidate>,
        mapping: Mapping,
        mapped: bool,
        mapped_verified: bool,
        second_router: bool,
        hostname: Option<String>,
        multi_use: bool,
        now_unix: u64,
    ) -> Result<Invite, BuildError> {
        let lifetime = if multi_use {
            MULTI_USE_SECS
        } else {
            SINGLE_USE_SECS
        };
        let candidates = candidates
            .into_iter()
            .map(|c| match c.addr {
                SocketAddr::V6(addr) => Candidate {
                    kind: c.kind,
                    addr: SocketAddr::V6(wire::without_scope(addr)),
                },
                SocketAddr::V4(_) => c,
            })
            .collect();
        let mut invite = Invite {
            host_key,
            invite_id: [0; 8],
            secret: [0; 16],
            multi_use,
            expires_at: now_unix.saturating_add(lifetime),
            candidates,
            mapping,
            mapped,
            mapped_verified,
            second_router,
            hostname,
        };
        invite.check()?;
        getrandom::fill(&mut invite.invite_id).map_err(BuildError::Random)?;
        getrandom::fill(&mut invite.secret).map_err(BuildError::Random)?;
        Ok(invite)
    }

    pub fn check(&self) -> Result<(), BuildError> {
        if self.mapped_verified && !self.mapped {
            return Err(BuildError::VerifiedWithoutMapping);
        }
        if self.candidates.len() > MAX_CANDIDATES {
            return Err(BuildError::TooManyCandidates(self.candidates.len()));
        }
        for candidate in &self.candidates {
            wire::check_candidate(candidate).map_err(|reason| BuildError::BadCandidate {
                candidate: *candidate,
                reason,
            })?;
        }
        if let Some(name) = &self.hostname {
            wire::check_hostname(name)?;
        }
        wire::check_expiry(self.expires_at)
    }

    // The fields are public, and an invite new() would have refused can still get here. In a
    // debug build that is a bug worth stopping on. A release build leaves out the parts decode
    // would refuse, and whatever text comes out still opens on the other side.
    pub fn encode(&self) -> String {
        debug_assert!(
            self.check().is_ok(),
            "encoding an invalid invite: {:?}",
            self.check()
        );
        let candidates: Vec<&Candidate> = self
            .candidates
            .iter()
            .filter(|c| wire::check_candidate(c).is_ok())
            .take(MAX_CANDIDATES)
            .collect();
        let hostname = self
            .hostname
            .as_deref()
            .filter(|name| wire::hostname_rule(name).is_ok());

        let mut flags = wire::mapping_bits(self.mapping) << MAPPING_SHIFT;
        for (on, bit) in [
            (self.multi_use, MULTI_USE),
            (self.mapped, MAPPED),
            (self.mapped && self.mapped_verified, MAPPED_VERIFIED),
            (self.second_router, SECOND_ROUTER),
            (hostname.is_some(), HAS_HOSTNAME),
        ] {
            if on {
                flags |= bit;
            }
        }

        let mut body = Vec::with_capacity(MAX_BODY);
        text::start(Kind::Invite, &mut body);
        body.push(flags);
        body.extend_from_slice(&self.host_key);
        body.extend_from_slice(&self.invite_id);
        body.extend_from_slice(&self.secret);
        body.extend_from_slice(&wire::expiry_bytes(self.expires_at));
        body.push(candidates.len() as u8);
        for candidate in candidates {
            wire::put_candidate(&mut body, candidate);
        }
        if let Some(name) = hostname {
            body.push(name.len() as u8);
            body.extend_from_slice(name.as_bytes());
        }
        let text = text::wrap(Kind::Invite, &mut body);
        wire::wipe(&mut body);
        text
    }

    pub fn decode(text: &str) -> Result<Invite, CodeError> {
        text::open(Kind::Invite, text, parse)
    }

    pub fn is_expired(&self, now_unix: u64) -> bool {
        now_unix >= self.expires_at
    }
}

fn parse(fields: &[u8]) -> Option<Invite> {
    let mut r = Reader::new(fields);
    let flags = r.u8()?;
    if flags & !KNOWN_FLAGS != 0 {
        return None;
    }
    if flags & MAPPED_VERIFIED != 0 && flags & MAPPED == 0 {
        return None;
    }
    let mapping = wire::mapping_from_bits((flags >> MAPPING_SHIFT) & 0b11)?;
    let host_key = r.array()?;
    let invite_id = r.array()?;
    // A borrow into the wiped buffer until the end: a refused code makes no copy.
    let secret = r.bytes(16)?;
    let expires_at = u64::from(r.u32()?);

    let count = usize::from(r.u8()?);
    if count > MAX_CANDIDATES {
        return None;
    }
    let mut candidates = Vec::with_capacity(count);
    for _ in 0..count {
        candidates.push(wire::read_candidate(&mut r)?);
    }

    let hostname = if flags & HAS_HOSTNAME != 0 {
        let len = usize::from(r.u8()?);
        let name = std::str::from_utf8(r.bytes(len)?).ok()?;
        wire::hostname_rule(name).ok()?;
        Some(name.to_owned())
    } else {
        None
    };

    if !r.is_empty() {
        return None;
    }
    Some(Invite {
        host_key,
        invite_id,
        secret: secret.try_into().ok()?,
        multi_use: flags & MULTI_USE != 0,
        expires_at,
        candidates,
        mapping,
        mapped: flags & MAPPED != 0,
        mapped_verified: flags & MAPPED_VERIFIED != 0,
        second_router: flags & SECOND_ROUTER != 0,
        hostname,
    })
}

impl Drop for Invite {
    fn drop(&mut self) {
        wire::wipe(&mut self.secret);
    }
}

impl fmt::Debug for Invite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Invite")
            .field("host_key", &Hex(&self.host_key))
            .field("invite_id", &Hex(&self.invite_id))
            .field("secret", &format_args!("(hidden)"))
            .field("multi_use", &self.multi_use)
            .field("expires_at", &self.expires_at)
            .field("candidates", &self.candidates)
            .field("mapping", &self.mapping)
            .field("mapped", &self.mapped)
            .field("mapped_verified", &self.mapped_verified)
            .field("second_router", &self.second_router)
            .field("hostname", &self.hostname)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CandidateKind, base32};
    use proptest::collection::vec;
    use proptest::prelude::*;
    use proptest::sample::Index;
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};

    fn sealed(fields: &[u8]) -> String {
        text::seal(Kind::Invite, fields)
    }

    // With every field in use, an edit can land in any part of the parser.
    fn full_fields() -> Vec<u8> {
        let v4 = |kind, a, b, c, d| Candidate {
            kind,
            addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(a, b, c, d), 41000)),
        };
        let v6 = |kind, last| Candidate {
            kind,
            addr: SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::new(0x2a02, 0x8071, 0, 0, 0, 0, 0, last),
                41000,
                0,
                0,
            )),
        };
        let invite = Invite {
            host_key: [0x11; 32],
            invite_id: [0x22; 8],
            secret: [0x33; 16],
            multi_use: true,
            expires_at: 1_800_000_000,
            candidates: vec![
                v4(CandidateKind::Lan, 192, 168, 1, 20),
                v4(CandidateKind::Vpn, 100, 64, 1, 2),
                v6(CandidateKind::Vpn, 1),
                v6(CandidateKind::Ipv6, 2),
                v4(CandidateKind::Public, 203, 0, 113, 9),
            ],
            mapping: Mapping::Easy,
            mapped: true,
            mapped_verified: true,
            second_router: true,
            hostname: Some("home.example.net".into()),
        };
        text::fields(Kind::Invite, &invite.encode())
    }

    fn assert_canonical(text: &str) -> Result<(), TestCaseError> {
        if let Ok(invite) = Invite::decode(text) {
            prop_assert!(invite.check().is_ok());
            prop_assert_eq!(invite.encode(), text);
        }
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4096))]

        #[test]
        fn random_body_without_checksum(body in vec(any::<u8>(), 0..800)) {
            let mut text = String::from("booth1-");
            base32::encode_into(&body, &mut text);
            let _ = Invite::decode(&text);
        }

        #[test]
        fn random_body_with_checksum(fields in vec(any::<u8>(), 0..800)) {
            assert_canonical(&sealed(&fields))?;
        }

        #[test]
        fn edited_body_with_checksum(edits in vec((any::<Index>(), any::<u8>(), 0..3u8), 1..8)) {
            let mut body = full_fields();
            for (at, byte, op) in edits {
                match op {
                    0 if !body.is_empty() => {
                        let i = at.index(body.len());
                        body[i] = byte;
                    }
                    1 => body.insert(at.index(body.len() + 1), byte),
                    _ if !body.is_empty() => {
                        body.remove(at.index(body.len()));
                    }
                    _ => {}
                }
            }
            assert_canonical(&sealed(&body))?;
        }
    }

    #[test]
    fn full_body_decodes() {
        assert!(Invite::decode(&sealed(&full_fields())).is_ok());
    }

    #[test]
    fn largest_invite_fits_the_limit() {
        let candidates = (1..=MAX_CANDIDATES as u16)
            .map(|i| Candidate {
                kind: CandidateKind::Ipv6,
                addr: SocketAddr::V6(SocketAddrV6::new(
                    Ipv6Addr::new(0x2a02, 0, 0, 0, 0, 0, 0, i),
                    41000,
                    0,
                    0,
                )),
            })
            .collect();
        let label = "a".repeat(63);
        let hostname = format!("{label}.{label}.{label}.{}", "b".repeat(61));
        assert_eq!(hostname.len(), wire::MAX_HOSTNAME);
        let invite = Invite::new(
            [1; 32],
            candidates,
            Mapping::Hard,
            false,
            false,
            false,
            Some(hostname),
            true,
            1_800_000_000,
        )
        .unwrap();
        let fields = text::fields(Kind::Invite, &invite.encode());
        assert_eq!(1 + PREAMBLE_LEN + fields.len() + CHECKSUM_LEN, MAX_BODY);
        assert_eq!(Invite::decode(&invite.encode()).unwrap(), invite);
    }
}
