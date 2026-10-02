use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6};

use crate::text::{self, CHECKSUM_LEN, Kind};
use crate::wire::{self, Reader};
use crate::{BuildError, CodeError, Mapping};

pub const REPLY_SECS: u64 = 5 * 60;

const REJOIN: u8 = 1 << 0;
const HAS_V4: u8 = 1 << 1;
const HAS_V6: u8 = 1 << 2;
const MAPPING_SHIFT: u8 = 3;
const KNOWN_FLAGS: u8 = 0x1f;

// layout, flags, invite id, client key, outside IPv4, outside IPv6, expiry, checksum
pub(crate) const MAX_BODY: usize = 1 + 1 + 8 + 32 + wire::V4_LEN + wire::V6_LEN + 4 + CHECKSUM_LEN;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Answers {
    Invite([u8; 8]),
    // The host already has the client's key from an earlier join, and client_key names it.
    Rejoin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReplyCode {
    pub answers: Answers,
    pub client_key: [u8; 32],
    pub outside_v4: Option<SocketAddrV4>,
    pub outside_v6: Option<SocketAddrV6>,
    pub mapping: Mapping,
    pub expires_at: u64,
}

impl ReplyCode {
    pub fn new(
        answers: Answers,
        client_key: [u8; 32],
        outside_v4: Option<SocketAddrV4>,
        outside_v6: Option<SocketAddrV6>,
        mapping: Mapping,
        now_unix: u64,
    ) -> Result<ReplyCode, BuildError> {
        let code = ReplyCode {
            answers,
            client_key,
            outside_v4,
            outside_v6: outside_v6.map(wire::without_scope),
            mapping,
            expires_at: now_unix.saturating_add(REPLY_SECS),
        };
        code.check()?;
        Ok(code)
    }

    pub fn check(&self) -> Result<(), BuildError> {
        if let Some(addr) = self.outside_v4 {
            wire::check_public_v4(addr).map_err(|reason| BuildError::BadAddress {
                addr: SocketAddr::V4(addr),
                reason,
            })?;
        }
        if let Some(addr) = self.outside_v6 {
            wire::check_public_v6(addr).map_err(|reason| BuildError::BadAddress {
                addr: SocketAddr::V6(addr),
                reason,
            })?;
        }
        wire::check_expiry(self.expires_at)
    }

    // Same rule as Invite::encode: stop in debug, and in release leave out what decode would
    // refuse.
    pub fn encode(&self) -> String {
        debug_assert!(
            self.check().is_ok(),
            "encoding an invalid reply code: {:?}",
            self.check()
        );
        let v4 = self
            .outside_v4
            .filter(|a| wire::check_public_v4(*a).is_ok());
        let v6 = self
            .outside_v6
            .filter(|a| wire::check_public_v6(*a).is_ok());

        let mut flags = wire::mapping_bits(self.mapping) << MAPPING_SHIFT;
        if self.answers == Answers::Rejoin {
            flags |= REJOIN;
        }
        if v4.is_some() {
            flags |= HAS_V4;
        }
        if v6.is_some() {
            flags |= HAS_V6;
        }

        let mut body = Vec::with_capacity(MAX_BODY);
        text::start(Kind::Reply, &mut body);
        body.push(flags);
        if let Answers::Invite(id) = self.answers {
            body.extend_from_slice(&id);
        }
        body.extend_from_slice(&self.client_key);
        if let Some(addr) = v4 {
            wire::put_v4(&mut body, addr);
        }
        if let Some(addr) = v6 {
            wire::put_v6(&mut body, addr);
        }
        body.extend_from_slice(&wire::expiry_bytes(self.expires_at));
        text::wrap(Kind::Reply, &mut body)
    }

    pub fn decode(text: &str) -> Result<ReplyCode, CodeError> {
        text::open(Kind::Reply, text, parse)
    }

    pub fn is_expired(&self, now_unix: u64) -> bool {
        now_unix >= self.expires_at
    }
}

fn parse(fields: &[u8]) -> Option<ReplyCode> {
    let mut r = Reader::new(fields);
    let flags = r.u8()?;
    if flags & !KNOWN_FLAGS != 0 {
        return None;
    }
    let mapping = wire::mapping_from_bits((flags >> MAPPING_SHIFT) & 0b11)?;
    let answers = if flags & REJOIN != 0 {
        Answers::Rejoin
    } else {
        Answers::Invite(r.array()?)
    };
    let client_key = r.array()?;
    let outside_v4 = if flags & HAS_V4 != 0 {
        let addr = r.v4()?;
        wire::check_public_v4(addr).ok()?;
        Some(addr)
    } else {
        None
    };
    let outside_v6 = if flags & HAS_V6 != 0 {
        let addr = r.v6()?;
        wire::check_public_v6(addr).ok()?;
        Some(addr)
    } else {
        None
    };
    let expires_at = u64::from(r.u32()?);
    if !r.is_empty() {
        return None;
    }
    Some(ReplyCode {
        answers,
        client_key,
        outside_v4,
        outside_v6,
        mapping,
        expires_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base32;
    use proptest::collection::vec;
    use proptest::prelude::*;
    use proptest::sample::Index;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn sealed(fields: &[u8]) -> String {
        text::seal(Kind::Reply, fields)
    }

    fn full_fields() -> Vec<u8> {
        let code = ReplyCode {
            answers: Answers::Invite([0x44; 8]),
            client_key: [0x55; 32],
            outside_v4: Some(SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 7), 52000)),
            outside_v6: Some(SocketAddrV6::new(
                Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 7),
                52000,
                0,
                0,
            )),
            mapping: Mapping::Easy,
            expires_at: 1_800_000_000,
        };
        let fields = text::fields(Kind::Reply, &code.encode());
        assert_eq!(1 + fields.len() + CHECKSUM_LEN, MAX_BODY);
        fields
    }

    fn assert_canonical(text: &str) -> Result<(), TestCaseError> {
        if let Ok(code) = ReplyCode::decode(text) {
            prop_assert!(code.check().is_ok());
            prop_assert_eq!(code.encode(), text);
        }
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4096))]

        #[test]
        fn random_body_without_checksum(body in vec(any::<u8>(), 0..200)) {
            let mut text = String::from("booth1-r-");
            base32::encode_into(&body, &mut text);
            let _ = ReplyCode::decode(&text);
        }

        #[test]
        fn random_body_with_checksum(fields in vec(any::<u8>(), 0..200)) {
            assert_canonical(&sealed(&fields))?;
        }

        #[test]
        fn edited_body_with_checksum(edits in vec((any::<Index>(), any::<u8>(), 0..3u8), 1..6)) {
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
}
