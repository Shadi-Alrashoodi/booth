// RFC 5389 binding requests over the program's one socket. Everything here that
// takes bytes takes them from the network: bounds-checked, no panics.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};

pub const MAGIC_COOKIE: u32 = 0x2112_A442;
pub const DEFAULT_SERVERS: [&str; 2] = ["stun.cloudflare.com:3478", "stun.l.google.com:19302"];

const DEFAULT_PORT: u16 = 3478;
const HEADER_LEN: usize = 20;

const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const BINDING_ERROR: u16 = 0x0111;

const MAPPED_ADDRESS: u16 = 0x0001;
const ERROR_CODE: u16 = 0x0009;
const XOR_MAPPED_ADDRESS: u16 = 0x0020;

const FAMILY_V4: u8 = 0x01;
const FAMILY_V6: u8 = 0x02;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mapping {
    Easy,
    Hard,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Ipv4,
    Ipv6,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StunError {
    NotStun,
    NotBindingResponse(u16),
    // The error code from the ERROR-CODE attribute, 0 when the server sent none.
    Refused(u16),
    Truncated,
    BadAddress,
    NoAddress,
}

impl fmt::Display for StunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StunError::NotStun => write!(f, "not a stun message"),
            StunError::NotBindingResponse(ty) => {
                write!(f, "stun message type 0x{ty:04x} is not a binding response")
            }
            StunError::Refused(0) => write!(f, "stun server refused the binding request"),
            StunError::Refused(code) => {
                write!(
                    f,
                    "stun server refused the binding request with error {code}"
                )
            }
            StunError::Truncated => write!(f, "stun attribute runs past the end of the message"),
            StunError::BadAddress => write!(f, "stun mapped address attribute is malformed"),
            StunError::NoAddress => write!(f, "stun binding response has no mapped address"),
        }
    }
}

impl std::error::Error for StunError {}

// Our own packet types start at 0x11, which also has the top two bits clear,
// so the cookie and the exact length are what really tell STUN apart.
pub fn is_stun(buf: &[u8]) -> bool {
    let (Some(first), Some(len), Some(cookie)) = (buf.first(), be16(buf, 2), be32(buf, 4)) else {
        return false;
    };
    let len = usize::from(len);
    first & 0xC0 == 0 && cookie == MAGIC_COOKIE && len % 4 == 0 && HEADER_LEN + len == buf.len()
}

pub fn new_transaction_id() -> [u8; 12] {
    let mut id = [0u8; 12];
    // ProcessPrng, which getrandom uses on Windows 10 and later, never fails.
    getrandom::fill(&mut id).expect("windows random number generator failed");
    id
}

pub fn binding_request(txid: &[u8; 12]) -> [u8; 20] {
    // No attributes, so the length field at bytes 2..4 stays zero.
    let mut msg = [0u8; HEADER_LEN];
    msg[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    msg[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    msg[8..20].copy_from_slice(txid);
    msg
}

pub fn parse_binding_response(buf: &[u8]) -> Result<([u8; 12], SocketAddr), StunError> {
    if !is_stun(buf) {
        return Err(StunError::NotStun);
    }
    let txid: [u8; 12] = buf
        .get(8..HEADER_LEN)
        .and_then(|id| id.try_into().ok())
        .ok_or(StunError::NotStun)?;
    let attrs = buf.get(HEADER_LEN..).ok_or(StunError::NotStun)?;

    match be16(buf, 0) {
        Some(BINDING_SUCCESS) => {}
        Some(BINDING_ERROR) => return Err(StunError::Refused(error_code(attrs))),
        Some(other) => return Err(StunError::NotBindingResponse(other)),
        None => return Err(StunError::NotStun),
    }

    let mut xor_mapped = None;
    let mut mapped = None;
    for attr in (Attributes { rest: attrs }) {
        let (ty, value) = attr?;
        match ty {
            XOR_MAPPED_ADDRESS if xor_mapped.is_none() => xor_mapped = Some(value),
            MAPPED_ADDRESS if mapped.is_none() => mapped = Some(value),
            _ => {}
        }
    }

    // MAPPED-ADDRESS is only read when the XOR form is missing, so an old or
    // odd one next to a good XOR-MAPPED-ADDRESS does not fail the answer.
    let addr = match (xor_mapped, mapped) {
        (Some(value), _) => decode_address(value, Some(&txid))?,
        (None, Some(value)) => decode_address(value, None)?,
        (None, None) => return Err(StunError::NoAddress),
    };
    Ok((txid, addr))
}

fn error_code(attrs: &[u8]) -> u16 {
    Attributes { rest: attrs }
        .map_while(Result::ok)
        .find(|(ty, _)| *ty == ERROR_CODE)
        .and_then(|(_, value)| Some((value.get(2)?, value.get(3)?)))
        .map_or(0, |(class, number)| {
            u16::from(class & 0x07) * 100 + u16::from(*number)
        })
}

fn decode_address(value: &[u8], xor_with: Option<&[u8; 12]>) -> Result<SocketAddr, StunError> {
    let family = value.get(1).copied().ok_or(StunError::BadAddress)?;
    let mut port = be16(value, 2).ok_or(StunError::BadAddress)?;
    let cookie = MAGIC_COOKIE.to_be_bytes();

    let ip = match (family, value.len()) {
        (FAMILY_V4, 8) => {
            let mut ip: [u8; 4] = value
                .get(4..8)
                .and_then(|b| b.try_into().ok())
                .ok_or(StunError::BadAddress)?;
            if xor_with.is_some() {
                xor_in_place(&mut ip, &cookie);
            }
            IpAddr::V4(Ipv4Addr::from(ip))
        }
        (FAMILY_V6, 20) => {
            let mut ip: [u8; 16] = value
                .get(4..20)
                .and_then(|b| b.try_into().ok())
                .ok_or(StunError::BadAddress)?;
            if let Some(txid) = xor_with {
                xor_in_place(&mut ip, &cookie);
                if let Some(tail) = ip.get_mut(4..) {
                    xor_in_place(tail, txid);
                }
            }
            IpAddr::V6(Ipv6Addr::from(ip))
        }
        _ => return Err(StunError::BadAddress),
    };
    if xor_with.is_some() {
        port ^= (MAGIC_COOKIE >> 16) as u16;
    }
    Ok(SocketAddr::new(ip, port))
}

fn xor_in_place(bytes: &mut [u8], key: &[u8]) {
    for (b, k) in bytes.iter_mut().zip(key) {
        *b ^= k;
    }
}

struct Attributes<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for Attributes<'a> {
    type Item = Result<(u16, &'a [u8]), StunError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        match split_attribute(self.rest) {
            Some((ty, value, rest)) => {
                self.rest = rest;
                Some(Ok((ty, value)))
            }
            None => {
                self.rest = &[];
                Some(Err(StunError::Truncated))
            }
        }
    }
}

// Values are padded to a multiple of 4 bytes; the padding is not part of the
// value but is part of the message.
fn split_attribute(buf: &[u8]) -> Option<(u16, &[u8], &[u8])> {
    let ty = be16(buf, 0)?;
    let len = usize::from(be16(buf, 2)?);
    let value = buf.get(4..4 + len)?;
    let rest = buf.get((4 + len).next_multiple_of(4)..)?;
    Some((ty, value, rest))
}

fn be16(buf: &[u8], at: usize) -> Option<u16> {
    let bytes = buf.get(at..at.checked_add(2)?)?;
    Some(u16::from_be_bytes(bytes.try_into().ok()?))
}

fn be32(buf: &[u8], at: usize) -> Option<u32> {
    let bytes = buf.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes(bytes.try_into().ok()?))
}

// Blocking: this asks the system resolver. A server typed without a port gets
// 3478, the standard STUN port.
pub fn resolve(server: &str) -> Vec<SocketAddr> {
    let server = server.trim();
    let bare_v6 = server
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(server);

    let found: Vec<SocketAddr> = if let Ok(ip) = bare_v6.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, DEFAULT_PORT)]
    } else if server.is_empty() {
        Vec::new()
    } else if !server.contains(':') {
        (server, DEFAULT_PORT)
            .to_socket_addrs()
            .map(Iterator::collect)
            .unwrap_or_default()
    } else {
        server
            .to_socket_addrs()
            .map(Iterator::collect)
            .unwrap_or_default()
    };

    let mut unique = Vec::with_capacity(found.len());
    for addr in found {
        if !unique.contains(&addr) {
            unique.push(addr);
        }
    }
    unique
}

// Takes (server, mapped) pairs as they arrived, both families mixed if need
// be; only pairs where server and mapped address are both of `family` count,
// so the result does not depend on which answer came back first.
pub fn classify(answers: &[(SocketAddr, SocketAddr)], family: Family) -> Mapping {
    let wanted = |addr: &SocketAddr| match family {
        Family::Ipv4 => addr.is_ipv4(),
        Family::Ipv6 => addr.is_ipv6(),
    };

    let mut per_server: Vec<(SocketAddr, SocketAddr)> = Vec::new();
    for &(server, mapped) in answers {
        if !wanted(&server) || !wanted(&mapped) {
            continue;
        }
        if !per_server.iter().any(|(s, _)| *s == server) {
            per_server.push((server, mapped));
        }
    }
    let Some(&(_, first_mapped)) = per_server.first() else {
        return Mapping::Unknown;
    };
    if per_server.len() < 2 {
        return Mapping::Unknown;
    }
    // A different outside IP means a load-balanced carrier NAT, which is as
    // useless for the reply code as a different port.
    if per_server.iter().any(|(_, mapped)| *mapped != first_mapped) {
        return Mapping::Hard;
    }
    // Two ports on one server IP cannot show a router that keys its mapping on
    // the destination address, so agreement only counts across server IPs.
    let first_server_ip = per_server.first().map(|(s, _)| s.ip());
    if per_server
        .iter()
        .any(|(s, _)| Some(s.ip()) != first_server_ip)
    {
        Mapping::Easy
    } else {
        Mapping::Unknown
    }
}
