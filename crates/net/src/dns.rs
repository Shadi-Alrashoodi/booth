// Finds the address behind an invite's name. The name's own nameservers are
// asked directly, so no resolver in between can hand back an old address; the
// system resolver finds those nameservers and stands in when they cannot be
// asked. Everything here blocks, for a helper thread.

use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::os::windows::io::{AsRawSocket, RawSocket};
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};
use std::{iter, ptr, slice, thread};

use socket2::{Domain, Protocol, Socket, Type};
use windows_sys::Win32::Foundation::{
    DNS_ERROR_RCODE_NAME_ERROR, DNS_INFO_NO_RECORDS, ERROR_SUCCESS,
};
use windows_sys::Win32::NetworkManagement::Dns::{
    DNS_QUERY_BYPASS_CACHE, DNS_QUERY_NO_MULTICAST, DNS_QUERY_NO_NETBT, DNS_QUERY_TREAT_AS_FQDN,
    DNS_RECORDA, DNS_RECORDW, DNS_TYPE_A, DNS_TYPE_AAAA, DNS_TYPE_CNAME, DNS_TYPE_NS, DnsFree,
    DnsFreeRecordList, DnsQuery_W, DnsSectionAnswer,
};
use windows_sys::Win32::Networking::WinSock::{
    SO_RANDOMIZE_PORT, SOCKET, SOCKET_ERROR, SOL_SOCKET, WSAEMSGSIZE, setsockopt,
};

use crate::adapters;
use crate::pcp::be16;
use crate::socket::last_wsa_error;

pub const PORT: u16 = 53;
pub const TIMEOUT: Duration = Duration::from_secs(1);
pub const MAX_SERVERS: usize = 4;
pub const MAX_CHAIN: usize = 8;

const HEADER_LEN: usize = 12;
const MAX_LABEL: usize = 63;
const MAX_NAME: usize = 255;
// Without EDNS a server keeps a UDP answer to 512 bytes. The rest is room to
// read an oversized one and judge it like any other.
const BUFFER: usize = 4096;
// How long past their own timeout the query threads get to report, so a
// scheduler hiccup does not turn an answer into a fallback.
const GRACE: Duration = Duration::from_millis(200);
// DNS names are far shorter; this only bounds the scan if Windows ever left a
// terminator out.
const MAX_WIDE: usize = 1024;
const ANSWER_SECTION: u32 = DnsSectionAnswer as u32;

const CLASS_IN: u16 = 1;
const QR: u16 = 0x8000;
const AA: u16 = 0x0400;
const TC: u16 = 0x0200;
const RD: u16 = 0x0100;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    A,
    Aaaa,
    Ns,
}

impl Kind {
    pub fn code(self) -> u16 {
        match self {
            Kind::A => DNS_TYPE_A,
            Kind::Aaaa => DNS_TYPE_AAAA,
            Kind::Ns => DNS_TYPE_NS,
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Kind::A => "A",
            Kind::Aaaa => "AAAA",
            Kind::Ns => "NS",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rcode(pub u8);

impl Rcode {
    pub const NOERROR: Rcode = Rcode(0);
    pub const FORMERR: Rcode = Rcode(1);
    pub const SERVFAIL: Rcode = Rcode(2);
    pub const NXDOMAIN: Rcode = Rcode(3);
    pub const NOTIMP: Rcode = Rcode(4);
    pub const REFUSED: Rcode = Rcode(5);

    pub fn name(self) -> &'static str {
        match self.0 {
            0 => "NOERROR",
            1 => "FORMERR",
            2 => "SERVFAIL",
            3 => "NXDOMAIN",
            4 => "NOTIMP",
            5 => "REFUSED",
            _ => "unknown rcode",
        }
    }
}

impl fmt::Display for Rcode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.name(), self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NameError {
    Empty,
    EmptyLabel,
    LongLabel(usize),
    Long(usize),
    Char(char),
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NameError::Empty => write!(f, "it is empty"),
            NameError::EmptyLabel => {
                write!(f, "it has two dots in a row or starts with a dot")
            }
            NameError::LongLabel(len) => {
                write!(
                    f,
                    "a part of it is {len} characters, over the {MAX_LABEL} dns allows"
                )
            }
            NameError::Long(len) => write!(
                f,
                "it is {len} characters, over the {} dns allows",
                MAX_NAME - 2
            ),
            NameError::Char(c) => write!(f, "{c:?} cannot be part of a dns name"),
        }
    }
}

impl std::error::Error for NameError {}

// Why a packet from a nameserver was not taken as the answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    Short(usize),
    NotAnswer,
    Id(u16),
    Opcode(u8),
    Questions(u16),
    Question,
    PastEnd,
    Pointer(usize),
    Label(u8),
    LongName,
    Record { kind: u16, len: usize },
    LongChain,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Short(len) => write!(f, "{len} bytes is shorter than a dns header"),
            ParseError::NotAnswer => write!(f, "it is a query, not an answer"),
            ParseError::Id(id) => write!(f, "id {id} is not the one asked"),
            ParseError::Opcode(op) => write!(f, "opcode {op} does not answer a query"),
            ParseError::Questions(count) => {
                write!(f, "it carries {count} questions, not the one asked")
            }
            ParseError::Question => write!(f, "its question is not the one asked"),
            ParseError::PastEnd => write!(f, "a name or record runs past the end"),
            ParseError::Pointer(target) => {
                write!(f, "a name pointer to offset {target} does not point back")
            }
            ParseError::Label(byte) => {
                write!(f, "{byte:#04x} is neither a label length nor a pointer")
            }
            ParseError::LongName => write!(f, "a name is over {MAX_NAME} bytes"),
            ParseError::Record { kind, len } => {
                write!(
                    f,
                    "a type {kind} record holds {len} bytes, which is not its size"
                )
            }
            ParseError::LongChain => write!(f, "a CNAME chain is longer than {MAX_CHAIN} steps"),
        }
    }
}

impl std::error::Error for ParseError {}

pub fn new_id() -> u16 {
    let mut id = [0u8; 2];
    // ProcessPrng, which getrandom uses on Windows 10 and later, never fails.
    getrandom::fill(&mut id).expect("windows random number generator failed");
    u16::from_ne_bytes(id)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    pub id: u16,
    pub kind: Kind,
    // Off when a nameserver is asked directly: it should answer from its own
    // data or not at all, never go and ask others on our behalf.
    pub recursion: bool,
    name: Vec<u8>,
}

impl Query {
    pub fn new(name: &str, kind: Kind, recursion: bool, id: u16) -> Result<Query, NameError> {
        Ok(Query {
            id,
            kind,
            recursion,
            name: wire_name(name)?,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let flags = if self.recursion { RD } else { 0 };
        let mut msg = Vec::with_capacity(HEADER_LEN + self.name.len() + 4);
        msg.extend_from_slice(&self.id.to_be_bytes());
        msg.extend_from_slice(&flags.to_be_bytes());
        // One question, no records.
        msg.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]);
        msg.extend_from_slice(&self.name);
        msg.extend_from_slice(&self.kind.code().to_be_bytes());
        msg.extend_from_slice(&CLASS_IN.to_be_bytes());
        msg
    }

    pub fn parse(&self, buf: &[u8]) -> Result<Response, ParseError> {
        let len = buf.len();
        let (Some(id), Some(flags), Some(questions), Some(answers), Some(authority), Some(extra)) = (
            be16(buf, 0),
            be16(buf, 2),
            be16(buf, 4),
            be16(buf, 6),
            be16(buf, 8),
            be16(buf, 10),
        ) else {
            return Err(ParseError::Short(len));
        };
        if flags & QR == 0 {
            return Err(ParseError::NotAnswer);
        }
        if id != self.id {
            return Err(ParseError::Id(id));
        }
        let opcode = ((flags >> 11) & 0xf) as u8;
        if opcode != 0 {
            return Err(ParseError::Opcode(opcode));
        }
        if questions != 1 {
            return Err(ParseError::Questions(questions));
        }
        let (name, at) = read_name(buf, HEADER_LEN)?;
        let (Some(kind), Some(class)) = (be16(buf, at), be16(buf, at + 2)) else {
            return Err(ParseError::PastEnd);
        };
        // Case is not part of a name, and some resolvers mix it on purpose.
        if !name.eq_ignore_ascii_case(&self.name) || kind != self.kind.code() || class != CLASS_IN {
            return Err(ParseError::Question);
        }

        let mut response = Response {
            authoritative: flags & AA != 0,
            truncated: flags & TC != 0,
            rcode: Rcode((flags & 0xf) as u8),
            answer_count: answers,
            addrs: Vec::new(),
            nameservers: Vec::new(),
            alias: None,
        };
        // What follows the question in a cut answer is whatever fitted, so
        // none of it counts.
        if response.truncated {
            return Ok(response);
        }
        // Each record takes 11 bytes at least, so a count far past the end
        // stops at the end, not after 65535 rounds.
        let mut at = at + 4;
        let mut records = Vec::new();
        for _ in 0..answers {
            let (record, next) = read_record(buf, at)?;
            records.push(record);
            at = next;
        }
        for _ in 0..u32::from(authority) + u32::from(extra) {
            at = read_record(buf, at)?.1;
        }
        if response.rcode == Rcode::NOERROR {
            self.follow(buf, &records, &mut response)?;
        }
        Ok(response)
    }

    // Takes the asked records for the asked name, through CNAMEs, from this
    // answer alone: a record for any other name is only as good as the zone
    // it came from, and this server may not be that zone's.
    fn follow(
        &self,
        buf: &[u8],
        records: &[Record],
        response: &mut Response,
    ) -> Result<(), ParseError> {
        let wanted = self.kind.code();
        let mut current = self.name.clone();
        let mut steps = 0;
        loop {
            let mut found = false;
            for record in records.iter().filter(|r| r.is(wanted, &current)) {
                found = true;
                match self.kind {
                    Kind::A => {
                        let octets: [u8; 4] = record.fixed(buf)?;
                        response.addrs.push(IpAddr::V4(Ipv4Addr::from(octets)));
                    }
                    Kind::Aaaa => {
                        let octets: [u8; 16] = record.fixed(buf)?;
                        response.addrs.push(IpAddr::V6(Ipv6Addr::from(octets)));
                    }
                    Kind::Ns => response.nameservers.push(text(&record.name(buf)?)),
                }
            }
            if found {
                return Ok(());
            }
            let Some(cname) = records.iter().find(|r| r.is(DNS_TYPE_CNAME, &current)) else {
                if steps > 0 {
                    response.alias = Some(text(&current));
                }
                return Ok(());
            };
            if steps == MAX_CHAIN {
                return Err(ParseError::LongChain);
            }
            steps += 1;
            current = cname.name(buf)?;
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub authoritative: bool,
    pub truncated: bool,
    pub rcode: Rcode,
    // Records of any name and type in the answer section. A server that sends
    // none and is not authoritative either has not answered at all: that is a
    // referral, or a server that does not serve the name.
    pub answer_count: u16,
    // The asked records for the asked name, through at most MAX_CHAIN CNAMEs
    // inside this answer: A and AAAA here, NS in `nameservers`.
    pub addrs: Vec<IpAddr>,
    pub nameservers: Vec<String>,
    // Where a CNAME chain led when that name's records were not in the answer.
    pub alias: Option<String>,
}

struct Record {
    owner: Vec<u8>,
    kind: u16,
    class: u16,
    data: usize,
    data_len: usize,
}

impl Record {
    fn is(&self, kind: u16, owner: &[u8]) -> bool {
        self.kind == kind && self.class == CLASS_IN && self.owner.eq_ignore_ascii_case(owner)
    }

    fn fixed<const N: usize>(&self, buf: &[u8]) -> Result<[u8; N], ParseError> {
        buf.get(self.data..self.data + self.data_len)
            .and_then(|data| data.try_into().ok())
            .ok_or(ParseError::Record {
                kind: self.kind,
                len: self.data_len,
            })
    }

    fn name(&self, buf: &[u8]) -> Result<Vec<u8>, ParseError> {
        let (name, end) = read_name(buf, self.data)?;
        if end != self.data + self.data_len {
            return Err(ParseError::Record {
                kind: self.kind,
                len: self.data_len,
            });
        }
        Ok(name)
    }
}

fn read_record(buf: &[u8], at: usize) -> Result<(Record, usize), ParseError> {
    let (owner, at) = read_name(buf, at)?;
    // Type, class, a TTL nothing here uses, then the data length.
    let (Some(kind), Some(class), Some(data_len)) =
        (be16(buf, at), be16(buf, at + 2), be16(buf, at + 8))
    else {
        return Err(ParseError::PastEnd);
    };
    let data = at + 10;
    let data_len = usize::from(data_len);
    let end = data + data_len;
    if end > buf.len() {
        return Err(ParseError::PastEnd);
    }
    Ok((
        Record {
            owner,
            kind,
            class,
            data,
            data_len,
        },
        end,
    ))
}

// A name as it stands in a message, pointers followed, returned in wire form
// with the offset just past it where it started. A pointer must land before
// the run of labels it ends, and each run starts before the last, so no loop
// can form and a hostile name ends after a few thousand steps at most.
fn read_name(buf: &[u8], start: usize) -> Result<(Vec<u8>, usize), ParseError> {
    let mut name = Vec::new();
    let mut pos = start;
    let mut run = start;
    let mut end = None;
    loop {
        let &byte = buf.get(pos).ok_or(ParseError::PastEnd)?;
        match byte & 0xc0 {
            0x00 if byte == 0 => {
                name.push(0);
                return Ok((name, end.unwrap_or(pos + 1)));
            }
            0x00 => {
                let label = buf
                    .get(pos + 1..pos + 1 + usize::from(byte))
                    .ok_or(ParseError::PastEnd)?;
                name.push(byte);
                name.extend_from_slice(label);
                // Room for the root label that still has to come.
                if name.len() + 1 > MAX_NAME {
                    return Err(ParseError::LongName);
                }
                pos += 1 + label.len();
            }
            0xc0 => {
                let &low = buf.get(pos + 1).ok_or(ParseError::PastEnd)?;
                let target = (usize::from(byte & 0x3f) << 8) | usize::from(low);
                if target < HEADER_LEN || target >= run {
                    return Err(ParseError::Pointer(target));
                }
                end.get_or_insert(pos + 2);
                run = target;
                pos = target;
            }
            _ => return Err(ParseError::Label(byte)),
        }
    }
}

fn wire_name(name: &str) -> Result<Vec<u8>, NameError> {
    let name = name.strip_suffix('.').unwrap_or(name);
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    let mut wire = Vec::with_capacity(name.len() + 2);
    for label in name.split('.') {
        if label.is_empty() {
            return Err(NameError::EmptyLabel);
        }
        if label.len() > MAX_LABEL {
            return Err(NameError::LongLabel(label.len()));
        }
        if let Some(c) = label
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_'))
        {
            return Err(NameError::Char(c));
        }
        wire.push(label.len() as u8);
        wire.extend_from_slice(label.as_bytes());
    }
    wire.push(0);
    if wire.len() > MAX_NAME {
        return Err(NameError::Long(name.len()));
    }
    Ok(wire)
}

// Dotted and lower case, with any byte a host name cannot hold written the
// way dig writes it, so a hostile name shows up in the log as what it is.
fn text(wire: &[u8]) -> String {
    let mut out = String::new();
    let mut rest = wire;
    while let Some((&len, tail)) = rest.split_first() {
        let Some(label) = tail.get(..usize::from(len)).filter(|l| !l.is_empty()) else {
            break;
        };
        if !out.is_empty() {
            out.push('.');
        }
        for &b in label {
            if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
                out.push(char::from(b.to_ascii_lowercase()));
            } else {
                out.push_str(&format!("\\{b:03}"));
            }
        }
        rest = tail.get(label.len()..).unwrap_or_default();
    }
    if out.is_empty() {
        out.push('.');
    }
    out
}

fn same_name(a: &str, b: &str) -> bool {
    let a = a.strip_suffix('.').unwrap_or(a);
    let b = b.strip_suffix('.').unwrap_or(b);
    a.eq_ignore_ascii_case(b)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SystemError {
    NoSuchName,
    Failed(String),
}

impl fmt::Display for SystemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SystemError::NoSuchName => write!(f, "the system resolver says no such name"),
            SystemError::Failed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for SystemError {}

// The system resolver, which finds the nameservers and stands in when they
// cannot be asked. Windows is the real one; tests hand in a fake so that
// nothing leaves loopback.
pub trait System {
    // NS records owned by exactly this name, empty when it has none of its own.
    fn nameservers(&self, name: &str) -> Result<Vec<String>, SystemError>;
    // A or AAAA records, CNAMEs followed. `fresh` skips Windows' own cache,
    // which may still hold the address from before a change.
    fn addresses(&self, name: &str, kind: Kind, fresh: bool) -> Result<Vec<IpAddr>, SystemError>;
}

#[derive(Clone, Copy, Debug)]
pub struct Windows;

impl System for Windows {
    fn nameservers(&self, name: &str) -> Result<Vec<String>, SystemError> {
        let mut names: Vec<String> = Vec::new();
        for record in system_query(name, Kind::Ns, false)? {
            if let SystemData::Name(ns) = record.data
                && record.answer
                && record.kind == DNS_TYPE_NS
                && same_name(&record.owner, name)
                && !ns.is_empty()
                && !names.iter().any(|seen| same_name(seen, &ns))
            {
                names.push(ns);
            }
        }
        Ok(names)
    }

    fn addresses(&self, name: &str, kind: Kind, fresh: bool) -> Result<Vec<IpAddr>, SystemError> {
        Ok(system_query(name, kind, fresh)?
            .into_iter()
            .filter(|record| record.answer && record.kind == kind.code())
            .filter_map(|record| match record.data {
                SystemData::Ip(ip) => Some(ip),
                SystemData::Name(_) | SystemData::Other => None,
            })
            .collect())
    }
}

struct SystemRecord {
    owner: String,
    kind: u16,
    answer: bool,
    data: SystemData,
}

enum SystemData {
    Ip(IpAddr),
    Name(String),
    Other,
}

// DnsQuery_W, with the record list copied into plain values and freed before
// returning, so every decision about the answer is made in safe code.
#[allow(unsafe_code)]
fn system_query(name: &str, kind: Kind, fresh: bool) -> Result<Vec<SystemRecord>, SystemError> {
    let wide: Vec<u16> = name.encode_utf16().chain(iter::once(0)).collect();
    // Suffixes, LLMNR and NetBIOS only fit names on the LAN, and a
    // single-label zone like "org" would otherwise go out to all three.
    let mut options = DNS_QUERY_TREAT_AS_FQDN | DNS_QUERY_NO_MULTICAST | DNS_QUERY_NO_NETBT;
    if fresh {
        options |= DNS_QUERY_BYPASS_CACHE;
    }
    let mut first: *mut DNS_RECORDA = ptr::null_mut();
    // SAFETY: `wide` is NUL-terminated and outlives the call, `first` is a
    // live pointer the call writes, and the two optional arguments are null.
    let rc = unsafe {
        DnsQuery_W(
            wide.as_ptr(),
            kind.code(),
            options,
            ptr::null_mut(),
            &mut first,
            ptr::null_mut(),
        )
    };

    let read = |p: *const u16| -> String {
        if p.is_null() {
            return String::new();
        }
        let mut len = 0;
        // SAFETY: names in the list are NUL-terminated and live until it is
        // freed below; the scan stops at the terminator or at MAX_WIDE.
        while len < MAX_WIDE && unsafe { *p.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: the `len` units just scanned are initialised and readable.
        String::from_utf16_lossy(unsafe { slice::from_raw_parts(p, len) })
    };
    let mut records = Vec::new();
    // The binding types the list as DNS_RECORDA; DnsQuery_W fills it with
    // DNS_RECORDW, the same layout with UTF-16 names.
    let mut cur = first.cast::<DNS_RECORDW>().cast_const();
    while !cur.is_null() {
        // SAFETY: `cur` is the head or a pNext written by DnsQuery_W, and the
        // list is freed only after this loop.
        let record = unsafe { &*cur };
        // SAFETY: both members of Flags are one u32; DW reads it whole.
        let section = unsafe { record.Flags.DW } & 0b11;
        let data = match (record.wType, record.wDataLength) {
            // SAFETY: wType and the matching length say which member of Data
            // DnsQuery_W filled.
            (DNS_TYPE_A, 4) => SystemData::Ip(IpAddr::V4(Ipv4Addr::from(
                unsafe { record.Data.A.IpAddress }.to_ne_bytes(),
            ))),
            // SAFETY: as above.
            (DNS_TYPE_AAAA, 16) => SystemData::Ip(IpAddr::V6(Ipv6Addr::from(unsafe {
                record.Data.AAAA.Ip6Address.IP6Byte
            }))),
            // SAFETY: as above.
            (DNS_TYPE_NS, _) => SystemData::Name(read(unsafe { record.Data.NS.pNameHost })),
            _ => SystemData::Other,
        };
        records.push(SystemRecord {
            owner: read(record.pName),
            kind: record.wType,
            answer: section == ANSWER_SECTION,
            data,
        });
        cur = record.pNext;
    }
    if !first.is_null() {
        // SAFETY: `first` came from DnsQuery_W, nothing read from the list is
        // still borrowed, and it is freed exactly once.
        unsafe { DnsFree(first.cast_const().cast(), DnsFreeRecordList) };
    }

    match rc {
        ERROR_SUCCESS => Ok(records),
        // No records of that type; what came back is the zone's SOA, which
        // the callers leave out.
        rc if rc == DNS_INFO_NO_RECORDS as u32 => Ok(records),
        DNS_ERROR_RCODE_NAME_ERROR => Err(SystemError::NoSuchName),
        rc => {
            let err = io::Error::from_raw_os_error(rc as i32);
            Err(SystemError::Failed(format!(
                "the system resolver could not look up the {kind} records of {name}: {err}"
            )))
        }
    }
}

// Where the name's own nameservers are, found once and kept for the life of
// the room: they rarely move, and finding them costs several lookups.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Nameservers {
    // The name the walk found NS records for: the name itself or one above it.
    pub zone: String,
    pub names: Vec<String>,
    // At most MAX_SERVERS, each passed the check and has a route from this
    // PC. One of each family per nameserver name, so neither one dead server
    // nor one broken family takes them all.
    pub addrs: Vec<SocketAddr>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    // The name's own nameserver, with the authoritative flag.
    Authoritative,
    // An answer without the flag: something between this PC and the
    // nameserver answered in its place, and may hand back an old address.
    NotAuthoritative,
    // Windows' resolver with its cache bypassed, which still depends on the
    // resolvers it asks in turn.
    System,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Source::Authoritative => "authoritative",
            Source::NotAuthoritative => "not authoritative",
            Source::System => "system resolver",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Found {
    pub ip: IpAddr,
    pub source: Source,
}

impl fmt::Display for Found {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.ip, self.source)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refused {
    pub ip: IpAddr,
    pub why: &'static str,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolved {
    // IPv4 first, each address once, every one passed the check.
    pub addrs: Vec<Found>,
    pub refused: Vec<Refused>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DnsError {
    Name { name: String, why: NameError },
    NoNameservers(String),
    NoServerAddress(String),
    NoSuchName(String),
    NoAddress(String),
    Unanswered(String),
}

impl fmt::Display for DnsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DnsError::Name { name, why } => {
                write!(f, "{name:?} is not a name that can be looked up: {why}")
            }
            DnsError::NoNameservers(name) => {
                write!(f, "found no nameservers for {name} or any name above it")
            }
            DnsError::NoServerAddress(zone) => {
                write!(
                    f,
                    "found nameservers for {zone} but no address to ask them at"
                )
            }
            DnsError::NoSuchName(name) => write!(f, "{name} does not exist"),
            DnsError::NoAddress(name) => write!(f, "{name} has no IPv4 or IPv6 address"),
            DnsError::Unanswered(name) => write!(
                f,
                "could not look up {name}: neither its nameservers nor the system resolver answered"
            ),
        }
    }
}

impl std::error::Error for DnsError {}

#[derive(Clone, Copy)]
pub struct Resolver<'a> {
    pub system: &'a dyn System,
    // Every nameserver address, before a query goes to it, and every address
    // an answer names must pass this. The room passes invite::check_addr,
    // which this crate cannot reach; tests on loopback pass one that lets
    // loopback through.
    pub check: &'a dyn Fn(SocketAddr) -> Result<(), &'static str>,
    // PORT, except in tests with a nameserver on loopback.
    pub port: u16,
    pub timeout: Duration,
}

impl<'a> Resolver<'a> {
    pub fn new(
        system: &'a dyn System,
        check: &'a dyn Fn(SocketAddr) -> Result<(), &'static str>,
    ) -> Resolver<'a> {
        Resolver {
            system,
            check,
            port: PORT,
            timeout: TIMEOUT,
        }
    }

    // Walks up from the name itself (myroom.duckdns.org, duckdns.org, org)
    // until the system resolver gives NS records, then finds their addresses.
    // Any failure on the way only moves the walk up a level: some dynamic DNS
    // providers run their own servers, which answer an NS question for a
    // host name in odd ways.
    pub fn nameservers(
        &self,
        name: &str,
        note: &mut dyn FnMut(fmt::Arguments<'_>),
    ) -> Result<Nameservers, DnsError> {
        let name = checked(name)?;
        let zones = iter::once(name.as_str()).chain(
            name.match_indices('.')
                .filter_map(|(dot, _)| name.get(dot + 1..)),
        );
        for zone in zones {
            note(format_args!(
                "asking the system resolver for the nameservers of {zone}"
            ));
            match self.system.nameservers(zone) {
                Ok(names) if names.is_empty() => {
                    note(format_args!("{zone} has no nameservers of its own"));
                }
                Ok(names) => {
                    note(format_args!("{zone} has nameservers {}", names.join(", ")));
                    return self.servers(zone, names, note);
                }
                Err(err) => note(format_args!("{zone}: {err}")),
            }
        }
        Err(DnsError::NoNameservers(name))
    }

    fn servers(
        &self,
        zone: &str,
        found: Vec<String>,
        note: &mut dyn FnMut(fmt::Arguments<'_>),
    ) -> Result<Nameservers, DnsError> {
        let mut names: Vec<String> = Vec::new();
        for ns in found {
            let ns = ns.strip_suffix('.').unwrap_or(&ns).to_ascii_lowercase();
            if !names.contains(&ns) {
                names.push(ns);
            }
        }
        let mut addrs = Vec::new();
        'names: for ns in &names {
            for kind in [Kind::A, Kind::Aaaa] {
                if addrs.len() == MAX_SERVERS {
                    break 'names;
                }
                match self.system.addresses(ns, kind, false) {
                    Ok(ips) if ips.is_empty() => {
                        note(format_args!("nameserver {ns} has no {kind} address"));
                    }
                    Ok(ips) => {
                        if let Some(server) = self.pick(ns, &ips, &addrs, note) {
                            addrs.push(server);
                        }
                    }
                    Err(err) => note(format_args!(
                        "could not find the {kind} address of nameserver {ns}: {err}"
                    )),
                }
            }
        }
        if addrs.is_empty() {
            return Err(DnsError::NoServerAddress(zone.to_string()));
        }
        note(format_args!("will ask {} directly", list(&addrs)));
        Ok(Nameservers {
            zone: zone.to_string(),
            names,
            addrs,
        })
    }

    fn pick(
        &self,
        ns: &str,
        ips: &[IpAddr],
        taken: &[SocketAddr],
        note: &mut dyn FnMut(fmt::Arguments<'_>),
    ) -> Option<SocketAddr> {
        for &ip in ips {
            let server = SocketAddr::new(ip, self.port);
            if taken.contains(&server) {
                continue;
            }
            if let Err(why) = (self.check)(server) {
                note(format_args!("refused nameserver {ns} at {server}: {why}"));
                continue;
            }
            // The list is one family, and a PC without a route to one global
            // address of it has none to the rest either.
            if let Err(err) = adapters::route_to(server) {
                note(format_args!(
                    "skipped nameserver {ns} at {server} and the rest of its addresses in that family: {err}"
                ));
                return None;
            }
            note(format_args!("nameserver {ns} is at {server}"));
            return Some(server);
        }
        None
    }

    // Asks the nameservers directly for A and AAAA at once, all of them in
    // parallel, and takes the first usable answer for each. The system
    // resolver, cache bypassed, stands in for a record type they could not
    // answer: port 53 blocked, a truncated answer, a CNAME into another zone.
    // Without nameservers it answers for both. Takes up to the timeout plus
    // whatever the system resolver takes.
    pub fn resolve(
        &self,
        name: &str,
        servers: Option<&Nameservers>,
        note: &mut dyn FnMut(fmt::Arguments<'_>),
    ) -> Result<Resolved, DnsError> {
        let name = checked(name)?;
        let direct = match servers {
            Some(servers) if !servers.addrs.is_empty() => {
                self.ask_directly(&name, &servers.addrs, note)
            }
            _ => {
                note(format_args!("no nameservers of {name} to ask directly"));
                [Direct::Fallback, Direct::Fallback]
            }
        };

        let mut resolved = Resolved::default();
        let mut missing = false;
        let mut empty = false;
        for (kind, direct) in [Kind::A, Kind::Aaaa].into_iter().zip(direct) {
            match direct {
                Direct::Answer(ips, source) => {
                    empty |= ips.is_empty();
                    self.take(&mut resolved, &ips, source, note);
                }
                Direct::NoSuchName => missing = true,
                Direct::Fallback => {
                    note(format_args!(
                        "asking the system resolver for the {kind} records of {name}, bypassing its cache"
                    ));
                    match self.system.addresses(&name, kind, true) {
                        Ok(ips) => {
                            note(format_args!("the system resolver gave {}", list(&ips)));
                            empty |= ips.is_empty();
                            self.take(&mut resolved, &ips, Source::System, note);
                        }
                        Err(SystemError::NoSuchName) => {
                            note(format_args!(
                                "the system resolver says {name} does not exist"
                            ));
                            missing = true;
                        }
                        Err(err) => note(format_args!("{err}")),
                    }
                }
            }
        }

        if !resolved.addrs.is_empty() || !resolved.refused.is_empty() {
            Ok(resolved)
        } else if missing {
            Err(DnsError::NoSuchName(name))
        } else if empty {
            Err(DnsError::NoAddress(name))
        } else {
            Err(DnsError::Unanswered(name))
        }
    }

    fn take(
        &self,
        resolved: &mut Resolved,
        ips: &[IpAddr],
        source: Source,
        note: &mut dyn FnMut(fmt::Arguments<'_>),
    ) {
        for &ip in ips {
            if resolved.addrs.iter().any(|found| found.ip == ip)
                || resolved.refused.iter().any(|refused| refused.ip == ip)
            {
                continue;
            }
            // A name carries an address, not a port. The check refuses port 0
            // and nothing else about a port, so any other one stands in.
            match (self.check)(SocketAddr::new(ip, self.port)) {
                Ok(()) => {
                    note(format_args!("took {ip} ({source})"));
                    resolved.addrs.push(Found { ip, source });
                }
                Err(why) => {
                    note(format_args!("refused {ip}: {why}"));
                    resolved.refused.push(Refused { ip, why });
                }
            }
        }
    }

    fn ask_directly(
        &self,
        name: &str,
        servers: &[SocketAddr],
        note: &mut dyn FnMut(fmt::Arguments<'_>),
    ) -> [Direct; 2] {
        let (events, heard) = mpsc::channel();
        let mut races = [Race::new(Kind::A), Race::new(Kind::Aaaa)];
        let mut allowed = Vec::with_capacity(servers.len());
        for &server in servers {
            match (self.check)(server) {
                Ok(()) => allowed.push(server),
                Err(why) => note(format_args!("refused nameserver {server}: {why}")),
            }
        }
        for (slot, race) in races.iter_mut().enumerate() {
            for &server in &allowed {
                let query = match Query::new(name, race.kind, false, new_id()) {
                    Ok(query) => query,
                    Err(why) => {
                        note(format_args!("could not ask for {name}: {why}"));
                        continue;
                    }
                };
                note(format_args!(
                    "asking {server} for the {} records of {name}, id {}",
                    race.kind, query.id
                ));
                let events = events.clone();
                let timeout = self.timeout;
                let spawned = thread::Builder::new()
                    .name("dns query".to_string())
                    .spawn(move || ask(slot, server, &query, timeout, &events));
                match spawned {
                    Ok(_) => race.pending += 1,
                    Err(err) => note(format_args!(
                        "could not start a thread to ask {server}: {err}"
                    )),
                }
            }
        }
        drop(events);

        let started = Instant::now();
        let wait = self.timeout.saturating_add(GRACE);
        while races
            .iter()
            .any(|race| race.outcome.is_none() && race.pending > 0)
        {
            let left = wait.saturating_sub(started.elapsed());
            let Ok(event) = heard.recv_timeout(left) else {
                break;
            };
            if let Some(race) = races.get_mut(event.slot) {
                race.hear(event, name, note);
            }
        }
        races.map(|race| {
            race.outcome.unwrap_or_else(|| {
                note(format_args!(
                    "no nameserver gave a usable {} answer for {name}",
                    race.kind
                ));
                Direct::Fallback
            })
        })
    }
}

fn checked(name: &str) -> Result<String, DnsError> {
    match wire_name(name) {
        Ok(_) => Ok(name.strip_suffix('.').unwrap_or(name).to_ascii_lowercase()),
        Err(why) => Err(DnsError::Name {
            name: name.to_string(),
            why,
        }),
    }
}

fn list<T: fmt::Display>(items: &[T]) -> String {
    if items.is_empty() {
        return "nothing".to_string();
    }
    items
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

enum Direct {
    // Possibly empty: the name exists and has no record of this type.
    Answer(Vec<IpAddr>, Source),
    NoSuchName,
    Fallback,
}

// One record type's race between the nameservers.
struct Race {
    kind: Kind,
    // Queries that have not yet answered, failed or run out of time.
    pending: usize,
    outcome: Option<Direct>,
}

impl Race {
    fn new(kind: Kind) -> Race {
        Race {
            kind,
            pending: 0,
            outcome: None,
        }
    }

    fn hear(&mut self, event: Event, name: &str, note: &mut dyn FnMut(fmt::Arguments<'_>)) {
        let kind = self.kind;
        let server = event.server;
        match event.heard {
            Heard::Ignored(why) => note(format_args!("ignored {why}")),
            Heard::Failed(why) => {
                self.pending = self.pending.saturating_sub(1);
                note(format_args!("{why}"));
            }
            Heard::Silent(waited) => {
                self.pending = self.pending.saturating_sub(1);
                note(format_args!(
                    "no {kind} answer from {server} within {} ms",
                    waited.as_millis()
                ));
            }
            Heard::Answer(response, took) => {
                self.pending = self.pending.saturating_sub(1);
                if self.outcome.is_some() {
                    note(format_args!(
                        "a later {kind} answer from {server}, not needed"
                    ));
                    return;
                }
                self.outcome = judge(kind, name, server, response, took, note);
            }
        }
    }
}

// What an answer that parsed means for the race: an outcome, or None to keep
// waiting for the other servers.
fn judge(
    kind: Kind,
    name: &str,
    server: SocketAddr,
    response: Response,
    took: Duration,
    note: &mut dyn FnMut(fmt::Arguments<'_>),
) -> Option<Direct> {
    let ms = took.as_secs_f64() * 1000.0;
    let source = if response.authoritative {
        Source::Authoritative
    } else {
        Source::NotAuthoritative
    };
    if response.truncated {
        note(format_args!(
            "the {kind} answer from {server} is truncated; asking the system resolver for {name} instead"
        ));
        return Some(Direct::Fallback);
    }
    // An address without the flag is still worth a handshake, which fails
    // at a wrong one. A no without it would end the lookup with nothing to
    // try, on the word of whatever answered in the nameserver's place, and
    // some networks answer that way for every dynamic DNS name.
    if response.rcode == Rcode::NXDOMAIN && response.authoritative {
        note(format_args!(
            "{server} says {name} does not exist ({source}, {ms:.1} ms)"
        ));
        return Some(Direct::NoSuchName);
    }
    if response.rcode == Rcode::NXDOMAIN {
        note(format_args!(
            "{server} says {name} does not exist, but not authoritatively: something between this PC and the nameserver answered in its place; not taken"
        ));
        return None;
    }
    if response.rcode != Rcode::NOERROR {
        note(format_args!(
            "{server} answered the {kind} question with {}",
            response.rcode
        ));
        return None;
    }
    if !response.addrs.is_empty() {
        note(format_args!(
            "{kind} answer from {server} after {ms:.1} ms, {source}: {}",
            list(&response.addrs)
        ));
        if !response.authoritative {
            note(format_args!(
                "the answer from {server} is not authoritative: something between this PC and the nameserver answered in its place, and the address may be old"
            ));
        }
        return Some(Direct::Answer(response.addrs, source));
    }
    if let Some(alias) = response.alias {
        note(format_args!(
            "{name} points on to {alias}, which the answer from {server} does not hold; asking the system resolver instead"
        ));
        return Some(Direct::Fallback);
    }
    if response.answer_count == 0 && !response.authoritative {
        note(format_args!(
            "{server} sent neither an answer nor authority for {name}: it does not serve that name"
        ));
        return None;
    }
    note(format_args!(
        "{server} says {name} has no {kind} record ({source}, {ms:.1} ms)"
    ));
    Some(Direct::Answer(Vec::new(), source))
}

struct Event {
    slot: usize,
    server: SocketAddr,
    heard: Heard,
}

enum Heard {
    // A packet that was not the answer, and the query goes on.
    Ignored(String),
    Answer(Response, Duration),
    Failed(String),
    Silent(Duration),
}

fn ask(slot: usize, server: SocketAddr, query: &Query, timeout: Duration, events: &Sender<Event>) {
    // A closed channel means the race is over; the rest is not needed.
    let tell = |heard| {
        let _ = events.send(Event {
            slot,
            server,
            heard,
        });
    };
    let heard = exchange(server, query, timeout, &mut |why| tell(Heard::Ignored(why)));
    tell(heard);
}

// One query from its own socket, answered or not within `timeout`. Only
// packets from `server` itself are read, and whatever does not parse as the
// answer to this query is reported and waited past.
fn exchange(
    server: SocketAddr,
    query: &Query,
    timeout: Duration,
    ignored: &mut dyn FnMut(String),
) -> Heard {
    let socket = match open(server) {
        Ok(socket) => socket,
        Err(err) => {
            return Heard::Failed(format!(
                "could not open a udp socket to nameserver {server}: {err}"
            ));
        }
    };
    if let Err(err) = socket.send(&query.encode()) {
        return Heard::Failed(format!("could not send to nameserver {server}: {err}"));
    }
    let sent = Instant::now();
    let mut buf = [0u8; BUFFER];
    loop {
        let left = timeout.saturating_sub(sent.elapsed());
        if left.is_zero() {
            return Heard::Silent(timeout);
        }
        if let Err(err) = socket.set_read_timeout(Some(left)) {
            return Heard::Failed(format!(
                "could not set the timeout on the socket to {server}: {err}"
            ));
        }
        match socket.recv_from(&mut buf) {
            Ok((len, from)) => {
                if from.ip() != server.ip() || from.port() != server.port() {
                    ignored(format!("{len} bytes from {from}: not the nameserver asked"));
                    continue;
                }
                match query.parse(buf.get(..len).unwrap_or_default()) {
                    Ok(response) => return Heard::Answer(response, sent.elapsed()),
                    Err(why) => ignored(format!("{len} bytes from {server}: {why}")),
                }
            }
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) => {}
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Heard::Failed(format!("nameserver {server} answered port unreachable"));
            }
            Err(err) if err.raw_os_error() == Some(WSAEMSGSIZE) => {
                ignored(format!("a packet from {server} over {BUFFER} bytes"));
            }
            Err(err) => {
                return Heard::Failed(format!(
                    "could not read the answer from nameserver {server}: {err}"
                ));
            }
        }
    }
}

// Left alone, Windows hands out ephemeral ports in order, and anyone who saw
// one query's port could guess the next; a forged answer would then only
// have the 16-bit id to get right. Connected, the socket takes nothing from
// any other source, and the source check in exchange is a second lock on
// the same door.
fn open(server: SocketAddr) -> io::Result<UdpSocket> {
    let socket = Socket::new(
        Domain::for_address(server),
        Type::DGRAM,
        Some(Protocol::UDP),
    )?;
    randomize_port(socket.as_raw_socket()).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("could not ask windows for a random source port: {err}"),
        )
    })?;
    socket.connect(&server.into())?;
    Ok(socket.into())
}

// Before connect, which is where the socket gets its port.
#[allow(unsafe_code)]
fn randomize_port(socket: RawSocket) -> io::Result<()> {
    let on: u32 = 1;
    // SAFETY: `socket` is an open socket the caller owns for the whole call,
    // and optval points at a live u32 whose size is passed as optlen.
    let rc = unsafe {
        setsockopt(
            socket as SOCKET,
            SOL_SOCKET,
            SO_RANDOMIZE_PORT,
            (&raw const on).cast(),
            size_of::<u32>() as i32,
        )
    };
    if rc == SOCKET_ERROR {
        return Err(last_wsa_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Without SO_RANDOMIZE_PORT these came out one after the other. At
    // random, two neighbours in a row out of seven steps is about one in
    // ten million.
    #[test]
    fn query_sockets_do_not_get_ports_in_order() {
        let server = SocketAddr::from((Ipv4Addr::LOCALHOST, 9));
        let sockets: Vec<UdpSocket> = (0..8).map(|_| open(server).unwrap()).collect();
        let ports: Vec<u16> = sockets
            .iter()
            .map(|socket| socket.local_addr().unwrap().port())
            .collect();
        let in_order = ports
            .windows(2)
            .filter(|pair| pair[1] == pair[0].wrapping_add(1))
            .count();
        assert!(in_order <= 1, "{ports:?}");
    }
}
