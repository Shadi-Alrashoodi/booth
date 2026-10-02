use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::ops::Deref;

use zeroize::Zeroize;

use crate::{BuildError, Candidate, CandidateKind, Mapping};

pub(crate) const MAX_HOSTNAME: usize = 253;
const MAX_LABEL: usize = 63;

pub(crate) const V4_LEN: usize = 4 + 2;
pub(crate) const V6_LEN: usize = 16 + 2;

const TAG_V4: u8 = 0x40;
const TAG_V6: u8 = 0x60;

pub(crate) struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Reader { rest: buf }
    }

    pub(crate) fn u8(&mut self) -> Option<u8> {
        let (&first, rest) = self.rest.split_first()?;
        self.rest = rest;
        Some(first)
    }

    pub(crate) fn bytes(&mut self, len: usize) -> Option<&'a [u8]> {
        let (head, rest) = self.rest.split_at_checked(len)?;
        self.rest = rest;
        Some(head)
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.bytes(N)?.try_into().ok()
    }

    pub(crate) fn u32(&mut self) -> Option<u32> {
        self.array().map(u32::from_be_bytes)
    }

    pub(crate) fn v4(&mut self) -> Option<SocketAddrV4> {
        let ip = Ipv4Addr::from(self.array::<4>()?);
        let port = u16::from_be_bytes(self.array()?);
        Some(SocketAddrV4::new(ip, port))
    }

    pub(crate) fn v6(&mut self) -> Option<SocketAddrV6> {
        let ip = Ipv6Addr::from(self.array::<16>()?);
        let port = u16::from_be_bytes(self.array()?);
        Some(SocketAddrV6::new(ip, port, 0, 0))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rest.is_empty()
    }
}

pub(crate) fn put_v4(out: &mut Vec<u8>, addr: SocketAddrV4) {
    out.extend_from_slice(&addr.ip().octets());
    out.extend_from_slice(&addr.port().to_be_bytes());
}

pub(crate) fn put_v6(out: &mut Vec<u8>, addr: SocketAddrV6) {
    out.extend_from_slice(&addr.ip().octets());
    out.extend_from_slice(&addr.port().to_be_bytes());
}

pub(crate) fn put_candidate(out: &mut Vec<u8>, candidate: &Candidate) {
    let kind = match candidate.kind {
        CandidateKind::Lan => 0,
        CandidateKind::Vpn => 1,
        CandidateKind::Ipv6 => 2,
        CandidateKind::Public => 3,
    };
    match candidate.addr {
        SocketAddr::V4(addr) => {
            out.push(TAG_V4 | kind);
            put_v4(out, addr);
        }
        SocketAddr::V6(addr) => {
            out.push(TAG_V6 | kind);
            put_v6(out, addr);
        }
    }
}

pub(crate) fn read_candidate(r: &mut Reader) -> Option<Candidate> {
    let tag = r.u8()?;
    let kind = match tag & 0x0f {
        0 => CandidateKind::Lan,
        1 => CandidateKind::Vpn,
        2 => CandidateKind::Ipv6,
        3 => CandidateKind::Public,
        _ => return None,
    };
    let addr = match tag & 0xf0 {
        TAG_V4 => SocketAddr::V4(r.v4()?),
        TAG_V6 => SocketAddr::V6(r.v6()?),
        _ => return None,
    };
    let candidate = Candidate { kind, addr };
    check_candidate(&candidate).ok()?;
    Some(candidate)
}

pub(crate) fn mapping_bits(mapping: Mapping) -> u8 {
    match mapping {
        Mapping::Unknown => 0,
        Mapping::Easy => 1,
        Mapping::Hard => 2,
    }
}

pub(crate) fn mapping_from_bits(bits: u8) -> Option<Mapping> {
    match bits {
        0 => Some(Mapping::Unknown),
        1 => Some(Mapping::Easy),
        2 => Some(Mapping::Hard),
        _ => None,
    }
}

// The rule for any address Booth sends to, whether it came from a code or from resolving the
// invite's host name: a real name can point at 127.0.0.1 as easily as a forged code can. An IPv4
// address written as IPv6 is refused; turn it back with to_canonical first.
pub fn check_addr(addr: SocketAddr) -> Result<(), &'static str> {
    match addr {
        SocketAddr::V4(addr) => usable_v4(addr),
        SocketAddr::V6(addr) => usable_v6(addr),
    }
}

fn usable_v4(addr: SocketAddrV4) -> Result<(), &'static str> {
    let ip = addr.ip();
    if addr.port() == 0 {
        Err("the port is 0")
    } else if ip.is_unspecified() {
        Err("it is the unspecified address")
    } else if ip.is_loopback() {
        Err("it is a loopback address")
    } else if ip.is_multicast() {
        Err("it is a multicast address")
    } else if ip.is_broadcast() {
        Err("it is the broadcast address")
    } else {
        Ok(())
    }
}

fn usable_v6(addr: SocketAddrV6) -> Result<(), &'static str> {
    let ip = addr.ip();
    if addr.port() == 0 {
        Err("the port is 0")
    } else if ip.is_unspecified() {
        Err("it is the unspecified address")
    } else if ip.is_loopback() {
        Err("it is a loopback address")
    } else if ip.is_multicast() {
        Err("it is a multicast address")
    } else if ip.to_ipv4_mapped().is_some() {
        Err("it is an IPv4 address written as IPv6")
    } else {
        Ok(())
    }
}

// A code has no room for these, so an address carrying them would not come back the same.
fn code_v6(addr: SocketAddrV6) -> Result<(), &'static str> {
    usable_v6(addr)?;
    if addr.scope_id() != 0 || addr.flowinfo() != 0 {
        return Err("it carries a scope id or flow label, which a code cannot hold");
    }
    Ok(())
}

// Public candidates and reply code addresses are what STUN saw, and STUN only ever sees
// addresses on the internet. Refusing the rest keeps a forged code from aiming the host's
// punch packets into its own network.
pub(crate) fn check_public_v4(addr: SocketAddrV4) -> Result<(), &'static str> {
    usable_v4(addr)?;
    match addr.ip().octets() {
        [10, ..] | [172, 16..=31, ..] | [192, 168, ..] => {
            Err("it is a private address, not one the internet can reach")
        }
        [100, 64..=127, ..] => {
            Err("it is a carrier-grade NAT address, not one the internet can reach")
        }
        [169, 254, ..] => Err("it is a link-local address"),
        [0, ..] | [240..=255, ..] => Err("it is a reserved address"),
        _ => Ok(()),
    }
}

pub(crate) fn check_public_v6(addr: SocketAddrV6) -> Result<(), &'static str> {
    code_v6(addr)?;
    match addr.ip().segments() {
        [first, ..] if first & 0xffc0 == 0xfe80 => Err("it is a link-local address"),
        [first, ..] if first & 0xfe00 == 0xfc00 => {
            Err("it is a unique local address, not one the internet can reach")
        }
        [0, 0, 0, 0, 0, 0, _, _] => Err("it is an old IPv4-compatible address"),
        [0x64, 0xff9b, 0, 0, 0, 0, _, _] => Err("it is a NAT64 address standing for an IPv4 one"),
        _ => Ok(()),
    }
}

pub(crate) fn check_candidate(candidate: &Candidate) -> Result<(), &'static str> {
    match (candidate.kind, candidate.addr) {
        (CandidateKind::Lan | CandidateKind::Vpn, SocketAddr::V4(addr)) => usable_v4(addr),
        (CandidateKind::Public, SocketAddr::V4(addr)) => check_public_v4(addr),
        (CandidateKind::Vpn, SocketAddr::V6(addr)) => code_v6(addr),
        (CandidateKind::Ipv6, SocketAddr::V6(addr)) => check_public_v6(addr),
        (CandidateKind::Lan | CandidateKind::Public, SocketAddr::V6(_)) => Err("it must be IPv4"),
        (CandidateKind::Ipv6, SocketAddr::V4(_)) => Err("it must be IPv6"),
    }
}

// Flow label and scope id mean nothing on the other PC, and the code does not carry them.
pub(crate) fn without_scope(addr: SocketAddrV6) -> SocketAddrV6 {
    SocketAddrV6::new(*addr.ip(), addr.port(), 0, 0)
}

pub(crate) fn hostname_rule(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("is empty");
    }
    if name.len() > MAX_HOSTNAME {
        return Err("is longer than 253 characters");
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
    {
        return Err("may contain only letters, digits, hyphens and dots");
    }
    for label in name.split('.') {
        if label.is_empty() {
            return Err("starts or ends with a dot, or has two dots in a row");
        }
        if label.len() > MAX_LABEL {
            return Err("has a part longer than 63 characters between dots");
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err("has a part that starts or ends with a hyphen");
        }
    }
    // Windows reads a name whose last part is a number as an IPv4 address ("127.1" and
    // "0x7f000001" are both loopback), and answers localhost itself, so either would bring back
    // the addresses the candidate rules refuse. No real top-level domain is numeric (RFC 3696).
    let last = name.rsplit('.').next().unwrap_or(name);
    if reads_as_number(last.as_bytes()) {
        return Err("ends in a number, so it would be read as an IP address");
    }
    if last.eq_ignore_ascii_case("localhost") {
        return Err("is localhost, which always means this same PC");
    }
    Ok(())
}

fn reads_as_number(label: &[u8]) -> bool {
    match label {
        [b'0', b'x' | b'X', hex @ ..] => hex.iter().all(u8::is_ascii_hexdigit),
        _ => label.iter().all(u8::is_ascii_digit),
    }
}

pub fn check_hostname(name: &str) -> Result<(), BuildError> {
    hostname_rule(name).map_err(BuildError::BadHostname)
}

// Expiry is stored as 32-bit unix seconds, which lasts until 2106 and keeps the invite short.
pub(crate) fn check_expiry(expires_at: u64) -> Result<(), BuildError> {
    if expires_at > u64::from(u32::MAX) {
        return Err(BuildError::ExpiryOutOfRange(expires_at));
    }
    Ok(())
}

pub(crate) fn expiry_bytes(expires_at: u64) -> [u8; 4] {
    u32::try_from(expires_at).unwrap_or(u32::MAX).to_be_bytes()
}

pub(crate) fn wipe(bytes: &mut [u8]) {
    bytes.zeroize();
}

// Pasted text and decoded bodies hold the invite secret. This buffer never grows, because growing
// frees the old allocation unwiped, and it wipes its whole allocation however it is dropped, so no
// return path leaves a copy on the heap. What it cannot reach: the BLAKE2s state that hashed the
// body (blake2 0.10 has no way to wipe it) and stack copies left by moves.
pub(crate) struct SecretBuf(Vec<u8>);

impl SecretBuf {
    pub(crate) fn with_capacity(len: usize) -> SecretBuf {
        SecretBuf(Vec::with_capacity(len))
    }

    pub(crate) fn push(&mut self, byte: u8) -> Option<()> {
        if self.0.len() == self.0.capacity() {
            return None;
        }
        self.0.push(byte);
        Some(())
    }
}

impl Deref for SecretBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for SecretBuf {
    fn drop(&mut self) {
        let whole = self.0.capacity();
        self.0.resize(whole, 0);
        wipe(&mut self.0);
    }
}

pub(crate) struct Hex<'a>(pub(crate) &'a [u8]);

impl fmt::Debug for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}
