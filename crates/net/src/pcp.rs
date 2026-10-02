// PCP (RFC 6887) MAP requests to the default gateway, UDP over IPv4 only,
// from a small socket of their own so router traffic never reaches the room's
// socket. NAT-PMP, which is PCP's version 0 on the same port, shares the
// send-and-wait loop at the bottom.

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

use windows_sys::Win32::Networking::WinSock::WSAEMSGSIZE;

pub const PORT: u16 = 5351;
pub const MAX_MESSAGE: usize = 1100;
pub const MAP_LEN: usize = 60;

const VERSION: u8 = 2;
const RESPONSE: u8 = 0x80;
const OPCODE_MAP: u8 = 1;
const HEADER_LEN: usize = 24;
const UDP: u8 = 17;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResultCode(pub u8);

impl ResultCode {
    pub const SUCCESS: ResultCode = ResultCode(0);
    pub const UNSUPP_VERSION: ResultCode = ResultCode(1);
    pub const NOT_AUTHORIZED: ResultCode = ResultCode(2);
    pub const MALFORMED_REQUEST: ResultCode = ResultCode(3);
    pub const UNSUPP_OPCODE: ResultCode = ResultCode(4);
    pub const UNSUPP_OPTION: ResultCode = ResultCode(5);
    pub const MALFORMED_OPTION: ResultCode = ResultCode(6);
    pub const NETWORK_FAILURE: ResultCode = ResultCode(7);
    pub const NO_RESOURCES: ResultCode = ResultCode(8);
    pub const UNSUPP_PROTOCOL: ResultCode = ResultCode(9);
    pub const USER_EX_QUOTA: ResultCode = ResultCode(10);
    pub const CANNOT_PROVIDE_EXTERNAL: ResultCode = ResultCode(11);
    pub const ADDRESS_MISMATCH: ResultCode = ResultCode(12);
    pub const EXCESSIVE_REMOTE_PEERS: ResultCode = ResultCode(13);

    pub fn name(self) -> &'static str {
        match self.0 {
            0 => "SUCCESS",
            1 => "UNSUPP_VERSION",
            2 => "NOT_AUTHORIZED",
            3 => "MALFORMED_REQUEST",
            4 => "UNSUPP_OPCODE",
            5 => "UNSUPP_OPTION",
            6 => "MALFORMED_OPTION",
            7 => "NETWORK_FAILURE",
            8 => "NO_RESOURCES",
            9 => "UNSUPP_PROTOCOL",
            10 => "USER_EX_QUOTA",
            11 => "CANNOT_PROVIDE_EXTERNAL",
            12 => "ADDRESS_MISMATCH",
            13 => "EXCESSIVE_REMOTE_PEERS",
            _ => "unknown result",
        }
    }
}

impl fmt::Display for ResultCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.name(), self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MapRequest {
    // Kept for the life of the mapping: the router refuses a renewal or a
    // deletion that does not carry the nonce which made it.
    pub nonce: [u8; 12],
    // Must also be the packet's source, or the router says ADDRESS_MISMATCH.
    pub client: Ipv4Addr,
    pub internal_port: u16,
    // 0.0.0.0 leaves the address to the router, port 0 the port.
    pub suggested: SocketAddrV4,
    // Seconds. Zero deletes the mapping.
    pub lifetime: u32,
}

// Shared with NAT-PMP, whose answer carries the same two things.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mapping {
    pub external: SocketAddrV4,
    // Seconds, as granted, which can be more or less than was asked for.
    pub lifetime: u32,
}

impl fmt::Display for Mapping {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} for {} s", self.external, self.lifetime)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Mapped(Mapping),
    Deleted,
    // On a refusal the lifetime is how long the router expects the same
    // request to keep failing.
    Refused { code: ResultCode, lifetime: u32 },
    // The router speaks another version, and 0 is NAT-PMP.
    UnsupportedVersion(u8),
}

// Why a packet from the router was not taken as the answer. NAT-PMP uses the
// same list; the PCP-only reasons never come up there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    Short(usize),
    Long(usize),
    Unaligned(usize),
    Version(u8),
    NotAnswer,
    Opcode(u8),
    Nonce,
    Protocol(u8),
    InternalPort(u16),
    BadOption,
    NotIpv4(Ipv6Addr),
    ZeroAddress,
    ZeroPort,
    ZeroLifetime,
    NotDeleted(u32),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Short(len) => write!(f, "{len} bytes is too short"),
            ParseError::Long(len) => {
                write!(f, "{len} bytes is over the {MAX_MESSAGE} byte limit")
            }
            ParseError::Unaligned(len) => write!(f, "{len} bytes is not a multiple of 4"),
            ParseError::Version(version) => write!(f, "version {version} is not ours"),
            ParseError::NotAnswer => write!(f, "it is a request, not an answer"),
            ParseError::Opcode(op) => write!(f, "opcode {op} does not answer ours"),
            ParseError::Nonce => write!(f, "the mapping nonce is not ours"),
            ParseError::Protocol(proto) => write!(f, "protocol {proto} is not udp"),
            ParseError::InternalPort(port) => write!(f, "internal port {port} is not ours"),
            ParseError::BadOption => write!(f, "an option runs past the end"),
            ParseError::NotIpv4(ip) => write!(f, "external address {ip} is not ipv4"),
            ParseError::ZeroAddress => write!(f, "external address is 0.0.0.0"),
            ParseError::ZeroPort => write!(f, "external port is 0"),
            ParseError::ZeroLifetime => write!(f, "granted lifetime is 0"),
            ParseError::NotDeleted(lifetime) => {
                write!(f, "lifetime {lifetime} s does not answer a deletion")
            }
        }
    }
}

impl std::error::Error for ParseError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PcpError {
    NoAnswer { tries: usize, waited: Duration },
    // Windows turns the router's ICMP port unreachable into this. NAT-PMP
    // listens on the same port, so it is not worth asking either.
    PortClosed,
    UnsupportedVersion(u8),
    Refused { code: ResultCode, lifetime: u32 },
    Io(String),
}

impl fmt::Display for PcpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PcpError::NoAnswer { tries, waited } => write!(
                f,
                "no pcp answer to {tries} requests in {} ms",
                waited.as_millis()
            ),
            PcpError::PortClosed => write!(
                f,
                "the router answered port unreachable: it has no pcp or nat-pmp service"
            ),
            PcpError::UnsupportedVersion(0) => write!(f, "the router speaks nat-pmp, not pcp"),
            PcpError::UnsupportedVersion(version) => {
                write!(f, "the router speaks pcp version {version}, not {VERSION}")
            }
            PcpError::Refused { code, lifetime: 0 } => {
                write!(f, "the router refused the pcp mapping: {code}")
            }
            PcpError::Refused { code, lifetime } => write!(
                f,
                "the router refused the pcp mapping: {code}, the same for the next {lifetime} s"
            ),
            PcpError::Io(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for PcpError {}

impl From<Unanswered> for PcpError {
    fn from(why: Unanswered) -> PcpError {
        match why {
            Unanswered::Silent { tries, waited } => PcpError::NoAnswer { tries, waited },
            Unanswered::PortClosed => PcpError::PortClosed,
            Unanswered::Io(message) => PcpError::Io(message),
        }
    }
}

pub fn new_nonce() -> [u8; 12] {
    let mut nonce = [0u8; 12];
    // ProcessPrng, which getrandom uses on Windows 10 and later, never fails.
    getrandom::fill(&mut nonce).expect("windows random number generator failed");
    nonce
}

pub fn map_request(request: &MapRequest) -> [u8; MAP_LEN] {
    let mut msg = [0u8; MAP_LEN];
    msg[0] = VERSION;
    msg[1] = OPCODE_MAP;
    msg[4..8].copy_from_slice(&request.lifetime.to_be_bytes());
    msg[8..24].copy_from_slice(&request.client.to_ipv6_mapped().octets());
    msg[24..36].copy_from_slice(&request.nonce);
    msg[36] = UDP;
    msg[40..42].copy_from_slice(&request.internal_port.to_be_bytes());
    msg[42..44].copy_from_slice(&request.suggested.port().to_be_bytes());
    msg[44..60].copy_from_slice(&request.suggested.ip().to_ipv6_mapped().octets());
    msg
}

pub fn parse_map_response(buf: &[u8], request: &MapRequest) -> Result<Answer, ParseError> {
    let len = buf.len();
    let (Some(&version), Some(&op), Some(&result)) = (buf.first(), buf.get(1), buf.get(3)) else {
        return Err(ParseError::Short(len));
    };
    if len > MAX_MESSAGE {
        return Err(ParseError::Long(len));
    }
    if !len.is_multiple_of(4) {
        return Err(ParseError::Unaligned(len));
    }
    if op & RESPONSE == 0 {
        return Err(ParseError::NotAnswer);
    }
    if op & !RESPONSE != OPCODE_MAP {
        return Err(ParseError::Opcode(op & !RESPONSE));
    }
    // A NAT-PMP router answers with its own 8-byte "unsupported version"
    // (RFC 6886 section 3.5): version 0, opcode 128 plus ours, and a 16-bit
    // result of 1, whose low byte sits where PCP keeps its result.
    let result = ResultCode(result);
    if result == ResultCode::UNSUPP_VERSION {
        return Ok(Answer::UnsupportedVersion(version));
    }
    if version != VERSION {
        return Err(ParseError::Version(version));
    }
    let lifetime = be32(buf, 4).ok_or(ParseError::Short(len))?;

    // An error answer to a request the router could not read may be the bare
    // header, with nothing to match against the request.
    if result != ResultCode::SUCCESS && len == HEADER_LEN {
        return Ok(Answer::Refused {
            code: result,
            lifetime,
        });
    }
    if len < MAP_LEN {
        return Err(ParseError::Short(len));
    }
    if buf.get(24..36) != Some(&request.nonce[..]) {
        return Err(ParseError::Nonce);
    }
    match buf.get(36) {
        Some(&UDP) => {}
        Some(&other) => return Err(ParseError::Protocol(other)),
        None => return Err(ParseError::Short(len)),
    }
    let internal_port = be16(buf, 40).ok_or(ParseError::Short(len))?;
    if internal_port != request.internal_port {
        return Err(ParseError::InternalPort(internal_port));
    }
    check_options(buf.get(MAP_LEN..).unwrap_or_default())?;

    if result != ResultCode::SUCCESS {
        return Ok(Answer::Refused {
            code: result,
            lifetime,
        });
    }
    // A late answer to an earlier map request looks the same apart from its
    // lifetime, and must not pass for the deletion's.
    if request.lifetime == 0 {
        return match lifetime {
            0 => Ok(Answer::Deleted),
            other => Err(ParseError::NotDeleted(other)),
        };
    }
    let port = be16(buf, 42).ok_or(ParseError::Short(len))?;
    let octets: [u8; 16] = buf
        .get(44..60)
        .and_then(|b| b.try_into().ok())
        .ok_or(ParseError::Short(len))?;
    let ip = Ipv6Addr::from(octets);
    let ip = ip.to_ipv4_mapped().ok_or(ParseError::NotIpv4(ip))?;
    if ip.is_unspecified() {
        return Err(ParseError::ZeroAddress);
    }
    if port == 0 {
        return Err(ParseError::ZeroPort);
    }
    if lifetime == 0 {
        return Err(ParseError::ZeroLifetime);
    }
    Ok(Answer::Mapped(Mapping {
        external: SocketAddrV4::new(ip, port),
        lifetime,
    }))
}

// Nothing we ask for comes back as an option, so they are only walked to
// make sure the framing holds.
fn check_options(mut rest: &[u8]) -> Result<(), ParseError> {
    while !rest.is_empty() {
        let len = usize::from(be16(rest, 2).ok_or(ParseError::BadOption)?);
        rest = rest
            .get((4 + len).next_multiple_of(4)..)
            .ok_or(ParseError::BadOption)?;
    }
    Ok(())
}

// One mapping on one router: our address and internal port stay fixed, and
// the nonce is kept so renewals and the deletion are accepted.
#[derive(Debug)]
pub struct Client {
    socket: UdpSocket,
    server: SocketAddr,
    local: Ipv4Addr,
    internal_port: u16,
    nonce: [u8; 12],
    granted: Option<SocketAddrV4>,
}

impl Client {
    // `server` is the gateway on PORT; tests hand it a loopback fake. The
    // socket is bound to `local` so the router sees the address we name.
    pub fn new(
        server: SocketAddr,
        local: Ipv4Addr,
        internal_port: u16,
        nonce: [u8; 12],
    ) -> io::Result<Client> {
        Ok(Client {
            socket: bind(local)?,
            server,
            local,
            internal_port,
            nonce,
            granted: None,
        })
    }

    // Each timeout is one try; the request goes out again after it passes.
    pub fn map(
        &mut self,
        suggested_port: u16,
        lifetime: u32,
        timeouts: &[Duration],
        note: &mut dyn FnMut(fmt::Arguments<'_>),
    ) -> Result<Mapping, PcpError> {
        let suggested = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, suggested_port);
        self.ask_for(suggested, lifetime, timeouts, note)
    }

    // Suggests what was granted last time, as RFC 6887 wants from a renewal,
    // so a router that lost its state still hands back the same port.
    pub fn renew(
        &mut self,
        lifetime: u32,
        timeouts: &[Duration],
        note: &mut dyn FnMut(fmt::Arguments<'_>),
    ) -> Result<Mapping, PcpError> {
        let suggested = self
            .granted
            .unwrap_or(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, self.internal_port));
        self.ask_for(suggested, lifetime, timeouts, note)
    }

    pub fn delete(
        &mut self,
        timeouts: &[Duration],
        note: &mut dyn FnMut(fmt::Arguments<'_>),
    ) -> Result<(), PcpError> {
        let suggested = self
            .granted
            .unwrap_or(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0));
        note(format_args!(
            "asking {} to delete the mapping for udp {}",
            self.server, self.internal_port
        ));
        match self.exchange(suggested, 0, timeouts, note)? {
            Answer::Deleted => {
                self.granted = None;
                Ok(())
            }
            other => Err(not_granted(other)),
        }
    }

    fn ask_for(
        &mut self,
        suggested: SocketAddrV4,
        lifetime: u32,
        timeouts: &[Duration],
        note: &mut dyn FnMut(fmt::Arguments<'_>),
    ) -> Result<Mapping, PcpError> {
        note(format_args!(
            "asking {} to map udp {}:{} to {suggested} for {lifetime} s",
            self.server, self.local, self.internal_port
        ));
        match self.exchange(suggested, lifetime, timeouts, note)? {
            Answer::Mapped(mapping) => {
                self.granted = Some(mapping.external);
                Ok(mapping)
            }
            other => Err(not_granted(other)),
        }
    }

    fn exchange(
        &self,
        suggested: SocketAddrV4,
        lifetime: u32,
        timeouts: &[Duration],
        note: &mut dyn FnMut(fmt::Arguments<'_>),
    ) -> Result<Answer, PcpError> {
        let request = MapRequest {
            nonce: self.nonce,
            client: self.local,
            internal_port: self.internal_port,
            suggested,
            lifetime,
        };
        let packet = map_request(&request);
        Ok(exchange(
            &self.socket,
            self.server,
            &packet,
            timeouts,
            note,
            |answer| parse_map_response(answer, &request),
        )?)
    }
}

fn not_granted(answer: Answer) -> PcpError {
    match answer {
        Answer::Refused { code, lifetime } => PcpError::Refused { code, lifetime },
        Answer::UnsupportedVersion(version) => PcpError::UnsupportedVersion(version),
        // The parser only says Deleted to a lifetime of 0, which is what
        // delete sends and map should not.
        Answer::Mapped(_) | Answer::Deleted => {
            PcpError::Io("a lifetime of 0 deletes a pcp mapping instead of making one".to_string())
        }
    }
}

pub(crate) fn bind(local: Ipv4Addr) -> io::Result<UdpSocket> {
    UdpSocket::bind(SocketAddrV4::new(local, 0)).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("could not bind a udp socket on {local} to ask the router for a port: {err}"),
        )
    })
}

pub(crate) enum Unanswered {
    Silent { tries: usize, waited: Duration },
    PortClosed,
    Io(String),
}

// Sends `request` once per timeout until `read` takes an answer. Only packets
// from `server` itself, address and port, are read at all; everything else,
// and whatever `read` turns down, is noted and waited past.
pub(crate) fn exchange<T, E: fmt::Display>(
    socket: &UdpSocket,
    server: SocketAddr,
    request: &[u8],
    timeouts: &[Duration],
    note: &mut dyn FnMut(fmt::Arguments<'_>),
    mut read: impl FnMut(&[u8]) -> Result<T, E>,
) -> Result<T, Unanswered> {
    // Room past the limit, so an oversized answer shows up as one.
    let mut buf = [0u8; MAX_MESSAGE + 4];
    let started = Instant::now();
    for (i, &wait) in timeouts.iter().enumerate() {
        let tries = i + 1;
        socket
            .send_to(request, server)
            .map_err(|err| Unanswered::Io(format!("could not send to {server}: {err}")))?;
        let sent = Instant::now();
        note(format_args!(
            "request {tries} of {} sent, waiting {} ms",
            timeouts.len(),
            wait.as_millis()
        ));
        let deadline = sent + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                note(format_args!(
                    "no answer to request {tries} within {} ms",
                    wait.as_millis()
                ));
                break;
            }
            socket.set_read_timeout(Some(left)).map_err(|err| {
                Unanswered::Io(format!("could not set the router socket timeout: {err}"))
            })?;
            match socket.recv_from(&mut buf) {
                Ok((len, from)) => {
                    if from != server {
                        note(format_args!(
                            "ignored {len} bytes from {from}: not the router"
                        ));
                        continue;
                    }
                    match read(buf.get(..len).unwrap_or_default()) {
                        Ok(answer) => {
                            note(format_args!(
                                "answer to request {tries} after {:.1} ms",
                                sent.elapsed().as_secs_f64() * 1000.0
                            ));
                            return Ok(answer);
                        }
                        Err(why) => note(format_args!("ignored {len} bytes from {from}: {why}")),
                    }
                }
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) => {}
                Err(err) if err.kind() == io::ErrorKind::ConnectionReset => {
                    return Err(Unanswered::PortClosed);
                }
                Err(err) if err.raw_os_error() == Some(WSAEMSGSIZE) => {
                    note(format_args!("ignored a packet over {} bytes", buf.len()));
                }
                Err(err) => {
                    return Err(Unanswered::Io(format!(
                        "could not read the answer from {server}: {err}"
                    )));
                }
            }
        }
    }
    Err(Unanswered::Silent {
        tries: timeouts.len(),
        waited: started.elapsed(),
    })
}

pub(crate) fn be16(buf: &[u8], at: usize) -> Option<u16> {
    let bytes = buf.get(at..at.checked_add(2)?)?;
    Some(u16::from_be_bytes(bytes.try_into().ok()?))
}

pub(crate) fn be32(buf: &[u8], at: usize) -> Option<u32> {
    let bytes = buf.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes(bytes.try_into().ok()?))
}
