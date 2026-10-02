// NAT-PMP (RFC 6886), for routers that answer PCP with "unsupported version"
// or not at all. Same port as PCP, same rules: a socket of its own bound to
// our address, answers read only from the gateway, every byte checked.

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Duration;

use crate::pcp::{self, MAX_MESSAGE, Unanswered, be16, be32};

pub use crate::pcp::{Mapping, PORT, ParseError};

pub const EXTERNAL_ADDRESS_LEN: usize = 12;
pub const MAP_LEN: usize = 16;

const VERSION: u8 = 0;
const RESPONSE: u8 = 128;
const OP_EXTERNAL_ADDRESS: u8 = 0;
const OP_MAP_UDP: u8 = 1;
// Version, opcode, result and the seconds since the router's epoch: all an
// error answer has to carry.
const ERROR_LEN: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResultCode(pub u16);

impl ResultCode {
    pub const SUCCESS: ResultCode = ResultCode(0);
    pub const UNSUPPORTED_VERSION: ResultCode = ResultCode(1);
    pub const NOT_AUTHORIZED: ResultCode = ResultCode(2);
    pub const NETWORK_FAILURE: ResultCode = ResultCode(3);
    pub const OUT_OF_RESOURCES: ResultCode = ResultCode(4);
    pub const UNSUPPORTED_OPCODE: ResultCode = ResultCode(5);

    pub fn name(self) -> &'static str {
        match self.0 {
            0 => "success",
            1 => "unsupported version",
            2 => "not authorized or refused",
            3 => "network failure",
            4 => "out of resources",
            5 => "unsupported opcode",
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
    pub internal_port: u16,
    // 0 leaves the port to the router, and a deletion must send 0.
    pub suggested_port: u16,
    // Seconds. Zero deletes the mapping.
    pub lifetime: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressAnswer {
    Address(Ipv4Addr),
    Refused(ResultCode),
    // A PCP-only router answers version 0 with its own version.
    UnsupportedVersion(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapAnswer {
    Mapped { port: u16, lifetime: u32 },
    Deleted,
    Refused(ResultCode),
    UnsupportedVersion(u8),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NatPmpError {
    NoAnswer { tries: usize, waited: Duration },
    PortClosed,
    UnsupportedVersion(u8),
    Refused(ResultCode),
    Io(String),
}

impl fmt::Display for NatPmpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NatPmpError::NoAnswer { tries, waited } => write!(
                f,
                "no nat-pmp answer to {tries} requests in {} ms",
                waited.as_millis()
            ),
            NatPmpError::PortClosed => write!(
                f,
                "the router answered port unreachable: it has no pcp or nat-pmp service"
            ),
            NatPmpError::UnsupportedVersion(version) => {
                write!(f, "the router speaks pcp version {version}, not nat-pmp")
            }
            NatPmpError::Refused(code) => {
                write!(f, "the router refused the nat-pmp request: {code}")
            }
            NatPmpError::Io(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for NatPmpError {}

impl From<Unanswered> for NatPmpError {
    fn from(why: Unanswered) -> NatPmpError {
        match why {
            Unanswered::Silent { tries, waited } => NatPmpError::NoAnswer { tries, waited },
            Unanswered::PortClosed => NatPmpError::PortClosed,
            Unanswered::Io(message) => NatPmpError::Io(message),
        }
    }
}

pub fn external_address_request() -> [u8; 2] {
    [VERSION, OP_EXTERNAL_ADDRESS]
}

pub fn map_request(request: &MapRequest) -> [u8; 12] {
    let mut msg = [0u8; 12];
    msg[0] = VERSION;
    msg[1] = OP_MAP_UDP;
    msg[4..6].copy_from_slice(&request.internal_port.to_be_bytes());
    msg[6..8].copy_from_slice(&request.suggested_port.to_be_bytes());
    msg[8..12].copy_from_slice(&request.lifetime.to_be_bytes());
    msg
}

pub fn parse_external_address(buf: &[u8]) -> Result<AddressAnswer, ParseError> {
    match header(buf, OP_EXTERNAL_ADDRESS)? {
        Header::Success => {}
        Header::Refused(code) => return Ok(AddressAnswer::Refused(code)),
        Header::UnsupportedVersion(version) => {
            return Ok(AddressAnswer::UnsupportedVersion(version));
        }
    }
    let octets: [u8; 4] = buf
        .get(8..EXTERNAL_ADDRESS_LEN)
        .and_then(|b| b.try_into().ok())
        .ok_or(ParseError::Short(buf.len()))?;
    let ip = Ipv4Addr::from(octets);
    // A router that has no address of its own yet says 0.0.0.0.
    if ip.is_unspecified() {
        return Err(ParseError::ZeroAddress);
    }
    Ok(AddressAnswer::Address(ip))
}

pub fn parse_map_response(buf: &[u8], request: &MapRequest) -> Result<MapAnswer, ParseError> {
    match header(buf, OP_MAP_UDP)? {
        Header::Success => {}
        Header::Refused(code) => return Ok(MapAnswer::Refused(code)),
        Header::UnsupportedVersion(version) => return Ok(MapAnswer::UnsupportedVersion(version)),
    }
    let len = buf.len();
    let (Some(internal_port), Some(port), Some(lifetime)) =
        (be16(buf, 8), be16(buf, 10), be32(buf, 12))
    else {
        return Err(ParseError::Short(len));
    };
    if internal_port != request.internal_port {
        return Err(ParseError::InternalPort(internal_port));
    }
    // A late answer to an earlier map request differs only in its lifetime.
    if request.lifetime == 0 {
        return match lifetime {
            0 => Ok(MapAnswer::Deleted),
            other => Err(ParseError::NotDeleted(other)),
        };
    }
    if port == 0 {
        return Err(ParseError::ZeroPort);
    }
    if lifetime == 0 {
        return Err(ParseError::ZeroLifetime);
    }
    Ok(MapAnswer::Mapped { port, lifetime })
}

enum Header {
    Success,
    Refused(ResultCode),
    UnsupportedVersion(u8),
}

// Longer answers than the RFC's are read up to its length; the RFC says
// nothing of trailing bytes, and they carry nothing we use.
fn header(buf: &[u8], opcode: u8) -> Result<Header, ParseError> {
    let len = buf.len();
    let (Some(&version), Some(&op), Some(result)) = (buf.first(), buf.get(1), be16(buf, 2)) else {
        return Err(ParseError::Short(len));
    };
    if len > MAX_MESSAGE {
        return Err(ParseError::Long(len));
    }
    if op & RESPONSE == 0 {
        return Err(ParseError::NotAnswer);
    }
    if op & !RESPONSE != opcode {
        return Err(ParseError::Opcode(op & !RESPONSE));
    }
    if version != VERSION {
        // PCP's result is the fourth byte, which is where the low byte of
        // NAT-PMP's 16-bit result sits.
        return if buf.get(3) == Some(&1) {
            Ok(Header::UnsupportedVersion(version))
        } else {
            Err(ParseError::Version(version))
        };
    }
    if len < ERROR_LEN {
        return Err(ParseError::Short(len));
    }
    match ResultCode(result) {
        ResultCode::SUCCESS => Ok(Header::Success),
        code => Ok(Header::Refused(code)),
    }
}

// One mapping on one router. NAT-PMP knows mappings by our address and
// internal port, so unlike PCP there is nothing to keep but the port granted.
#[derive(Debug)]
pub struct Client {
    socket: UdpSocket,
    server: SocketAddr,
    local: Ipv4Addr,
    internal_port: u16,
    granted_port: Option<u16>,
}

impl Client {
    // `server` is the gateway on PORT; tests hand it a loopback fake. The
    // router maps for the packet's source, so the socket is bound to `local`.
    pub fn new(server: SocketAddr, local: Ipv4Addr, internal_port: u16) -> io::Result<Client> {
        Ok(Client {
            socket: pcp::bind(local)?,
            server,
            local,
            internal_port,
            granted_port: None,
        })
    }

    pub fn external_address(
        &self,
        timeouts: &[Duration],
        note: &mut dyn FnMut(fmt::Arguments<'_>),
    ) -> Result<Ipv4Addr, NatPmpError> {
        note(format_args!(
            "asking {} for its external address",
            self.server
        ));
        let answer = pcp::exchange(
            &self.socket,
            self.server,
            &external_address_request(),
            timeouts,
            note,
            parse_external_address,
        )?;
        match answer {
            AddressAnswer::Address(ip) => Ok(ip),
            AddressAnswer::Refused(code) => Err(NatPmpError::Refused(code)),
            AddressAnswer::UnsupportedVersion(version) => {
                Err(NatPmpError::UnsupportedVersion(version))
            }
        }
    }

    // The map answer has no address in it, so the external address is asked
    // for first, with the same timeouts.
    pub fn map(
        &mut self,
        suggested_port: u16,
        lifetime: u32,
        timeouts: &[Duration],
        note: &mut dyn FnMut(fmt::Arguments<'_>),
    ) -> Result<Mapping, NatPmpError> {
        let ip = self.external_address(timeouts, note)?;
        note(format_args!(
            "asking {} to map udp {}:{} to port {suggested_port} for {lifetime} s",
            self.server, self.local, self.internal_port
        ));
        match self.exchange(suggested_port, lifetime, timeouts, note)? {
            MapAnswer::Mapped { port, lifetime } => {
                self.granted_port = Some(port);
                Ok(Mapping {
                    external: SocketAddrV4::new(ip, port),
                    lifetime,
                })
            }
            other => Err(not_granted(other)),
        }
    }

    // Asks again for the port granted last time. The address is asked for
    // again too, since it is the one thing that tells a renewal it changed.
    pub fn renew(
        &mut self,
        lifetime: u32,
        timeouts: &[Duration],
        note: &mut dyn FnMut(fmt::Arguments<'_>),
    ) -> Result<Mapping, NatPmpError> {
        let port = self.granted_port.unwrap_or(self.internal_port);
        self.map(port, lifetime, timeouts, note)
    }

    pub fn delete(
        &mut self,
        timeouts: &[Duration],
        note: &mut dyn FnMut(fmt::Arguments<'_>),
    ) -> Result<(), NatPmpError> {
        note(format_args!(
            "asking {} to delete the mapping for udp {}",
            self.server, self.internal_port
        ));
        match self.exchange(0, 0, timeouts, note)? {
            MapAnswer::Deleted => {
                self.granted_port = None;
                Ok(())
            }
            other => Err(not_granted(other)),
        }
    }

    fn exchange(
        &self,
        suggested_port: u16,
        lifetime: u32,
        timeouts: &[Duration],
        note: &mut dyn FnMut(fmt::Arguments<'_>),
    ) -> Result<MapAnswer, NatPmpError> {
        let request = MapRequest {
            internal_port: self.internal_port,
            suggested_port,
            lifetime,
        };
        Ok(pcp::exchange(
            &self.socket,
            self.server,
            &map_request(&request),
            timeouts,
            note,
            |answer| parse_map_response(answer, &request),
        )?)
    }
}

fn not_granted(answer: MapAnswer) -> NatPmpError {
    match answer {
        MapAnswer::Refused(code) => NatPmpError::Refused(code),
        MapAnswer::UnsupportedVersion(version) => NatPmpError::UnsupportedVersion(version),
        // The parser only says Deleted to a lifetime of 0, which is what
        // delete sends and map should not.
        MapAnswer::Mapped { .. } | MapAnswer::Deleted => NatPmpError::Io(
            "a lifetime of 0 deletes a nat-pmp mapping instead of making one".to_string(),
        ),
    }
}
