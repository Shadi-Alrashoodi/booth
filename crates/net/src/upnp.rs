// A small UPnP IGD client: the SSDP search, the router's description, and the
// four SOAP calls a port mapping needs. It talks only to the default gateway:
// an answer from any other address, or a URL naming any other host, is
// ignored. Every byte from the router is hostile input, read with bounds
// checks and size caps. It blocks, and runs on the room's mapper thread.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::str;
use std::time::{Duration, Instant};

use socket2::SockRef;
use windows_sys::Win32::Networking::WinSock::WSAEMSGSIZE;

pub const SSDP: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900));
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(2);
pub const MAX_ANSWER: usize = 64 * 1024;
pub const LEASE: u32 = 7200;
pub const MAPPING_NAME: &str = "Booth";

const GATEWAY_TYPES: [&str; 2] = [
    "urn:schemas-upnp-org:device:InternetGatewayDevice:1",
    "urn:schemas-upnp-org:device:InternetGatewayDevice:2",
];
const SSDP_BUFFER: usize = 8192;
// A lost search or a lost answer, which on Wi-Fi is not rare, would
// otherwise leave the room without a mapping. The UPnP architecture asks
// for more than one search too.
const SEARCH_AGAIN_AFTER: Duration = Duration::from_millis(300);
// Answers with different descriptions from the gateway's address. The router
// sends one, and more mean someone else on the LAN answers in its name.
const MAX_LOCATIONS: usize = 4;
// A LAN full of chatty devices, or someone flooding answers, must not fill
// the log. Past this many only the count is noted.
const NOTED_IGNORES: usize = 8;
const MAX_URL: usize = 512;
const MAX_VALUE: usize = 1024;
const MAX_DEPTH: usize = 64;
const MAX_SERVICES: usize = 32;
const MAX_VALUES: usize = 32;
const RANDOM_PORTS: usize = 3;
const DYNAMIC_PORTS: u16 = 49152;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    WanIp1,
    WanIp2,
    WanPpp1,
}

impl Kind {
    const ALL: [Kind; 3] = [Kind::WanIp1, Kind::WanIp2, Kind::WanPpp1];

    pub fn urn(self) -> &'static str {
        match self {
            Kind::WanIp1 => "urn:schemas-upnp-org:service:WANIPConnection:1",
            Kind::WanIp2 => "urn:schemas-upnp-org:service:WANIPConnection:2",
            Kind::WanPpp1 => "urn:schemas-upnp-org:service:WANPPPConnection:1",
        }
    }

    fn from_urn(text: &str) -> Option<Kind> {
        let text = text.trim();
        Kind::ALL
            .into_iter()
            .find(|kind| kind.urn().eq_ignore_ascii_case(text))
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let urn = self.urn();
        f.write_str(
            urn.strip_prefix("urn:schemas-upnp-org:service:")
                .unwrap_or(urn),
        )
    }
}

// Only ever built by Url::parse or joined onto one, so the address is the
// gateway's and the path is safe to put in a request line as it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Url {
    addr: SocketAddrV4,
    path: String,
}

impl Url {
    // http:// with the gateway's own address as a literal, an optional port,
    // and a path of printable ASCII. A host name or user info could point
    // anywhere, and https is refused because there is no certificate a
    // router could show that we could check.
    pub fn parse(text: &str, gateway: Ipv4Addr) -> Option<Url> {
        let text = text.trim();
        if text.len() > MAX_URL {
            return None;
        }
        let rest = strip_prefix_ignore_case(text, "http://")?;
        let (authority, path) = match rest.find('/') {
            Some(at) => rest.split_at(at),
            None => (rest, "/"),
        };
        let (host, port) = match authority.split_once(':') {
            Some((host, port)) => (host, port.parse::<u16>().ok().filter(|p| *p != 0)?),
            None => (authority, 80),
        };
        let ip: Ipv4Addr = host.parse().ok()?;
        if ip != gateway {
            return None;
        }
        Some(Url {
            addr: SocketAddrV4::new(ip, port),
            path: clean_path(path)?,
        })
    }

    pub fn addr(&self) -> SocketAddrV4 {
        self.addr
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    fn join(&self, reference: &str) -> Option<Url> {
        let reference = reference.trim();
        if reference.is_empty() {
            return None;
        }
        if strip_prefix_ignore_case(reference, "http://").is_some() {
            return Url::parse(reference, *self.addr.ip());
        }
        if let Some(rest) = reference.strip_prefix("//") {
            return Url::parse(&format!("http://{rest}"), *self.addr.ip());
        }
        // Any other scheme, https included.
        let first_segment = reference.split('/').next().unwrap_or_default();
        if first_segment.contains(':') {
            return None;
        }
        let path = if reference.starts_with('/') {
            reference.to_owned()
        } else {
            let base = self.path.split(['?', '#']).next().unwrap_or_default();
            let dir = base
                .rfind('/')
                .and_then(|at| base.get(..=at))
                .unwrap_or("/");
            format!("{dir}{reference}")
        };
        Some(Url {
            addr: self.addr,
            path: clean_path(&path)?,
        })
    }
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "http://{}{}", self.addr, self.path)
    }
}

fn clean_path(path: &str) -> Option<String> {
    let path = path.split_once('#').map_or(path, |(path, _)| path);
    let usable = path.starts_with('/')
        && path.len() <= MAX_URL
        && path.bytes().all(|b| b.is_ascii_graphic());
    usable.then(|| path.to_owned())
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| text.get(prefix.len()..))
        .flatten()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Service {
    pub kind: Kind,
    pub control: Url,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Router {
    pub service: Service,
    pub external_ip: Ipv4Addr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mapped {
    pub external_port: u16,
    // 0 when the router keeps only permanent mappings, or when the port was
    // already forwarded to us and that forward has no lease.
    pub lease: u32,
    // False when the router already had a forward to us that Booth did not
    // make, most likely the user's own: it is not ours to remove.
    pub delete_on_close: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub internal: SocketAddrV4,
    pub enabled: bool,
    pub lease: u32,
    // The router's text, printable ASCII only and cut short, for the log.
    pub description: String,
}

// Whose an entry on the router is, as far as the room can tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    // It forwards to us under Booth's name: the room's own, or a lease a
    // run that was killed left behind.
    Booth,
    // An enabled forward to us under another name, most likely set up by
    // hand.
    ThisPc,
    // Another device's, or a forward to us that is switched off.
    Other,
}

impl Entry {
    pub fn owner(&self, internal: SocketAddrV4) -> Owner {
        if self.internal != internal {
            Owner::Other
        } else if self.description.eq_ignore_ascii_case(MAPPING_NAME) {
            Owner::Booth
        } else if self.enabled {
            Owner::ThisPc
        } else {
            Owner::Other
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    GetExternalIpAddress,
    AddPortMapping,
    GetSpecificPortMappingEntry,
    DeletePortMapping,
}

impl Action {
    pub fn name(self) -> &'static str {
        match self {
            Action::GetExternalIpAddress => "GetExternalIPAddress",
            Action::AddPortMapping => "AddPortMapping",
            Action::GetSpecificPortMappingEntry => "GetSpecificPortMappingEntry",
            Action::DeletePortMapping => "DeletePortMapping",
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

// The errorCode from a SOAP fault.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fault(pub u16);

impl Fault {
    pub const NO_SUCH_ENTRY: Fault = Fault(714);
    pub const CONFLICT: Fault = Fault(718);
    pub const ONLY_PERMANENT_LEASES: Fault = Fault(725);

    // Spelled as the UPnP and IGD specs spell them, so a log line can be
    // looked up.
    pub fn name(self) -> Option<&'static str> {
        let name = match self.0 {
            401 => "InvalidAction",
            402 => "InvalidArgs",
            501 => "ActionFailed",
            600 => "ArgumentValueInvalid",
            601 => "ArgumentValueOutOfRange",
            602 => "OptionalActionNotImplemented",
            605 => "StringArgumentTooLong",
            606 => "ActionNotAuthorized",
            713 => "SpecifiedArrayIndexInvalid",
            714 => "NoSuchEntryInArray",
            715 => "WildCardNotPermittedInSrcIP",
            716 => "WildCardNotPermittedInExtPort",
            718 => "ConflictInMappingEntry",
            724 => "SamePortValuesRequired",
            725 => "OnlyPermanentLeasesSupported",
            726 => "RemoteHostOnlySupportsWildcard",
            727 => "ExternalPortOnlySupportsWildcard",
            728 => "NoPortMapsAvailable",
            729 => "ConflictWithOtherMechanisms",
            730 => "PortMappingNotFound",
            731 => "ReadOnly",
            732 => "WildCardNotPermittedInIntPort",
            733 => "InconsistentParameters",
            _ => return None,
        };
        Some(name)
    }
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(name) => write!(f, "{} {name}", self.0),
            None => write!(f, "{}", self.0),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ignored {
    NotFromGateway,
    NotAnAnswer,
    NotAGateway(String),
    NoLocation,
    LocationElsewhere(String),
    LocationQuery(String),
}

impl fmt::Display for Ignored {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ignored::NotFromGateway => write!(f, "it did not come from the gateway"),
            Ignored::NotAnAnswer => write!(f, "it is not an http 200 answer"),
            Ignored::NotAGateway(st) => write!(f, "it is for {st}, not an internet gateway"),
            Ignored::NoLocation => write!(f, "it has no LOCATION"),
            Ignored::LocationElsewhere(location) => {
                write!(
                    f,
                    "its LOCATION {location} is not http:// on the gateway's own address"
                )
            }
            Ignored::LocationQuery(location) => write!(
                f,
                "its LOCATION {location} has a query, which no router description needs"
            ),
        }
    }
}

#[derive(Debug)]
pub enum UpnpError {
    Bind(Ipv4Addr, io::Error),
    Send(SocketAddr, io::Error),
    Receive(io::Error),
    NoAnswer(Ipv4Addr),
    Connect(SocketAddrV4, io::Error),
    Timeout(SocketAddrV4),
    Io(SocketAddrV4, io::Error),
    BadHttp(&'static str),
    TooLarge,
    Status(u16),
    BadXml(&'static str),
    NoService(Url),
    Fault(Action, Fault),
    BadAnswer(Action, &'static str),
    NoFreePort(u16),
}

impl fmt::Display for UpnpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UpnpError::Bind(local, err) => {
                write!(
                    f,
                    "could not open a udp socket on {local} for the upnp search: {err}"
                )
            }
            UpnpError::Send(to, err) => write!(f, "could not send the upnp search to {to}: {err}"),
            UpnpError::Receive(err) => write!(f, "the upnp search socket failed: {err}"),
            UpnpError::NoAnswer(gateway) => {
                write!(f, "the router at {gateway} did not answer the upnp search")
            }
            UpnpError::Connect(addr, err) => {
                write!(f, "could not connect to the router at {addr}: {err}")
            }
            UpnpError::Timeout(addr) => write!(
                f,
                "the router at {addr} did not answer within {} s",
                HTTP_TIMEOUT.as_secs()
            ),
            UpnpError::Io(addr, err) => {
                write!(f, "the connection to the router at {addr} failed: {err}")
            }
            UpnpError::BadHttp(what) => write!(f, "the router sent a broken http answer: {what}"),
            UpnpError::TooLarge => write!(
                f,
                "the router's answer is larger than {} KB",
                MAX_ANSWER / 1024
            ),
            UpnpError::Status(code) => write!(f, "the router answered with http status {code}"),
            UpnpError::BadXml(why) => write!(f, "could not read the router's xml: {why}"),
            UpnpError::NoService(location) => write!(
                f,
                "the description at {location} lists no WANIPConnection or WANPPPConnection service on the gateway"
            ),
            UpnpError::Fault(action, fault) => {
                write!(f, "the router refused {action} with error {fault}")
            }
            UpnpError::BadAnswer(action, what) => {
                write!(f, "the router's answer to {action} has no usable {what}")
            }
            UpnpError::NoFreePort(port) => write!(
                f,
                "port {port} and {RANDOM_PORTS} random ports are all taken on the router"
            ),
        }
    }
}

impl std::error::Error for UpnpError {}

// Search, read the description, and take the first listed service that tells
// us its external address. Each answer from the gateway's address is tried
// in the order it came, so a bad one costs one failed description.
pub fn find(
    gateway: Ipv4Addr,
    local: Ipv4Addr,
    to: SocketAddr,
    wait: Duration,
    note: &mut dyn FnMut(fmt::Arguments<'_>),
) -> Result<Router, UpnpError> {
    let mut search = Search::start(gateway, local, to, wait, note)?;
    let mut last = UpnpError::NoAnswer(gateway);
    while let Some(location) = search.next(note)? {
        match router_at(&location, note) {
            Ok(router) => return Ok(router),
            Err(err) => {
                note(format_args!(
                    "the description at {location} led nowhere: {err}"
                ));
                last = err;
            }
        }
    }
    Err(last)
}

// A router with a PPP and an IP connection often lists both, and only the
// one in use has an address.
fn router_at(
    location: &Url,
    note: &mut dyn FnMut(fmt::Arguments<'_>),
) -> Result<Router, UpnpError> {
    let services = describe(location)?;
    let mut last = UpnpError::NoService(location.clone());
    for service in services {
        match external_ip(&service) {
            Ok(external_ip) => {
                note(format_args!(
                    "{} at {} says the external address is {external_ip}",
                    service.kind, service.control
                ));
                return Ok(Router {
                    service,
                    external_ip,
                });
            }
            Err(err) => {
                note(format_args!(
                    "{} at {}: {err}",
                    service.kind, service.control
                ));
                last = err;
            }
        }
    }
    Err(last)
}

// The SSDP search, kept open while find() tries what it hands out. Nothing
// checks the source of a UDP packet on a LAN and the search is multicast,
// so any device there can answer in the router's name, and faster.
pub struct Search {
    socket: UdpSocket,
    to: SocketAddr,
    gateway: Ipv4Addr,
    again_at: Option<Instant>,
    deadline: Instant,
    taken: Vec<Url>,
    ignored: usize,
    ignored_noted: usize,
    buf: Vec<u8>,
}

impl Search {
    // `to` is SSDP in real use; tests point it at a responder on loopback.
    pub fn start(
        gateway: Ipv4Addr,
        local: Ipv4Addr,
        to: SocketAddr,
        wait: Duration,
        note: &mut dyn FnMut(fmt::Arguments<'_>),
    ) -> Result<Search, UpnpError> {
        let socket = UdpSocket::bind((local, 0)).map_err(|err| UpnpError::Bind(local, err))?;
        // Without this Windows sends the multicast out of whichever adapter
        // it likes best, which on a PC with Hyper-V or a VPN is often not
        // the LAN.
        if to.ip().is_multicast() {
            SockRef::from(&socket)
                .set_multicast_if_v4(&local)
                .map_err(|err| UpnpError::Bind(local, err))?;
        }
        let started = Instant::now();
        let deadline = started + wait;
        let search = Search {
            socket,
            to,
            gateway,
            again_at: Some(started + SEARCH_AGAIN_AFTER).filter(|at| *at < deadline),
            deadline,
            taken: Vec::new(),
            ignored: 0,
            ignored_noted: 0,
            buf: vec![0u8; SSDP_BUFFER],
        };
        search.send()?;
        note(format_args!(
            "sent the ssdp search for gateway devices 1 and 2 from {local} to {to}"
        ));
        Ok(search)
    }

    // The next answer from the gateway's address whose description has not
    // been handed out yet. None once the wait is over, or after
    // MAX_LOCATIONS. Past the wait, what arrived while an earlier answer's
    // description was being read is still looked at.
    pub fn next(
        &mut self,
        note: &mut dyn FnMut(fmt::Arguments<'_>),
    ) -> Result<Option<Url>, UpnpError> {
        let found = self.read(note);
        let past = self.ignored.saturating_sub(NOTED_IGNORES);
        if past > self.ignored_noted {
            note(format_args!(
                "ignored {} more ssdp answers",
                past - self.ignored_noted
            ));
            self.ignored_noted = past;
        }
        let Some((from, location)) = found? else {
            if self.taken.is_empty() {
                return Err(UpnpError::NoAnswer(self.gateway));
            }
            return Ok(None);
        };
        note(format_args!(
            "took the ssdp answer from {from}: description at {location}"
        ));
        Ok(Some(location))
    }

    fn send(&self) -> Result<(), UpnpError> {
        for st in GATEWAY_TYPES {
            let request = format!(
                "M-SEARCH * HTTP/1.1\r\nHOST: {}\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: {st}\r\n\r\n",
                self.to
            );
            self.socket
                .send_to(request.as_bytes(), self.to)
                .map_err(|err| UpnpError::Send(self.to, err))?;
        }
        Ok(())
    }

    fn read(
        &mut self,
        note: &mut dyn FnMut(fmt::Arguments<'_>),
    ) -> Result<Option<(SocketAddr, Url)>, UpnpError> {
        while self.taken.len() < MAX_LOCATIONS {
            let now = Instant::now();
            if self.again_at.is_some_and(|at| now >= at) {
                self.again_at = None;
                self.send()?;
                note(format_args!("sent the ssdp search again"));
            }
            let over = now >= self.deadline;
            if over {
                self.socket
                    .set_nonblocking(true)
                    .map_err(UpnpError::Receive)?;
            } else {
                let until = self.again_at.unwrap_or(self.deadline);
                self.socket
                    .set_read_timeout(Some(until.saturating_duration_since(now)))
                    .map_err(UpnpError::Receive)?;
            }
            match self.socket.recv_from(&mut self.buf) {
                Ok((len, from)) => {
                    let datagram = self.buf.get(..len).unwrap_or_default();
                    match ssdp_answer(datagram, from, self.gateway) {
                        // The router answers once per device type, and again
                        // to the second search.
                        Ok(location) if self.taken.contains(&location) => {}
                        Ok(location) => {
                            self.taken.push(location.clone());
                            return Ok(Some((from, location)));
                        }
                        Err(why) => self.ignore(&from, &why, note),
                    }
                }
                // Windows fills the buffer and reports the rest as lost,
                // without saying who sent it.
                Err(err) if err.raw_os_error() == Some(WSAEMSGSIZE) => self.ignore(
                    &"an unknown sender",
                    &format_args!("it is longer than {SSDP_BUFFER} bytes"),
                    note,
                ),
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    if over {
                        return Ok(None);
                    }
                }
                // An ICMP port unreachable for an earlier send surfaces here
                // on Windows; it says nothing about the search.
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::ConnectionReset | io::ErrorKind::Interrupted
                    ) => {}
                Err(err) => return Err(UpnpError::Receive(err)),
            }
        }
        Ok(None)
    }

    fn ignore(
        &mut self,
        from: &dyn fmt::Display,
        why: &dyn fmt::Display,
        note: &mut dyn FnMut(fmt::Arguments<'_>),
    ) {
        self.ignored += 1;
        if self.ignored <= NOTED_IGNORES {
            note(format_args!("ignored an ssdp answer from {from}: {why}"));
        }
    }
}

pub fn ssdp_answer(datagram: &[u8], from: SocketAddr, gateway: Ipv4Addr) -> Result<Url, Ignored> {
    if from.ip().to_canonical() != IpAddr::V4(gateway) {
        return Err(Ignored::NotFromGateway);
    }
    let text = String::from_utf8_lossy(datagram);
    let mut lines = text.lines();
    if lines.next().and_then(status_code) != Some(200) {
        return Err(Ignored::NotAnAnswer);
    }
    let mut location = None;
    let mut st = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.eq_ignore_ascii_case("location") {
            location.get_or_insert(value.trim());
        } else if name.eq_ignore_ascii_case("st") {
            st.get_or_insert(value.trim());
        }
    }
    if let Some(st) = st
        && !GATEWAY_TYPES.iter().any(|t| t.eq_ignore_ascii_case(st))
    {
        return Err(Ignored::NotAGateway(printable(st, 80)));
    }
    let location = location.ok_or(Ignored::NoLocation)?;
    let url = Url::parse(location, gateway)
        .ok_or_else(|| Ignored::LocationElsewhere(printable(location, 80)))?;
    // Whoever answers picks the path the host fetches from the router, and
    // a query is what a request to the router's own admin pages would need.
    if url.path.contains('?') {
        return Err(Ignored::LocationQuery(printable(location, 80)));
    }
    Ok(url)
}

pub fn describe(location: &Url) -> Result<Vec<Service>, UpnpError> {
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        location.path, location.addr
    );
    let answer = exchange(location.addr, request.as_bytes())?;
    if answer.status != 200 {
        return Err(UpnpError::Status(answer.status));
    }
    services(&answer.body, location)
}

// The WANIPConnection and WANPPPConnection services in a description, in
// document order, with their control URLs made absolute. Services whose URL
// leads off the gateway are left out.
pub fn services(xml: &[u8], location: &Url) -> Result<Vec<Service>, UpnpError> {
    enum Field {
        ServiceType,
        ControlUrl,
        UrlBase,
    }

    if xml.len() > MAX_ANSWER {
        return Err(UpnpError::TooLarge);
    }
    let mut doc = Doc::new(xml);
    let mut value: Option<(Field, String)> = None;
    let mut service: Option<(Option<String>, Option<String>)> = None;
    let mut url_base = None;
    let mut listed = Vec::new();

    while let Some(event) = doc.next_event().map_err(UpnpError::BadXml)? {
        match event {
            Event::Open(name) => {
                if value.is_some() {
                    return Err(UpnpError::BadXml("an element inside a url or service type"));
                }
                if is(name, "service") {
                    if service.is_some() {
                        return Err(UpnpError::BadXml("a service inside a service"));
                    }
                    service = Some((None, None));
                } else if service.is_some() && doc.parent().is_some_and(|p| is(p, "service")) {
                    if is(name, "serviceType") {
                        value = Some((Field::ServiceType, String::new()));
                    } else if is(name, "controlURL") {
                        value = Some((Field::ControlUrl, String::new()));
                    }
                } else if is(name, "URLBase") && doc.depth() == 2 {
                    value = Some((Field::UrlBase, String::new()));
                }
            }
            Event::Text(raw) => {
                if let Some((_, text)) = &mut value {
                    decode(raw, text).map_err(UpnpError::BadXml)?;
                }
            }
            Event::Cdata(raw) => {
                if let Some((_, text)) = &mut value {
                    append_cdata(raw, text).map_err(UpnpError::BadXml)?;
                }
            }
            Event::Close(name) => {
                // Nothing opens inside a value, so a close while one is open
                // is that value's own element.
                if let Some((field, text)) = value.take() {
                    match (field, &mut service) {
                        (Field::ServiceType, Some((ty, _))) => *ty = Some(text),
                        (Field::ControlUrl, Some((_, control))) => *control = Some(text),
                        (Field::UrlBase, _) => url_base = Some(text),
                        _ => {}
                    }
                } else if is(name, "service")
                    && let Some((Some(ty), Some(control))) = service.take()
                    && let Some(kind) = Kind::from_urn(&ty)
                    && listed.len() < MAX_SERVICES
                {
                    listed.push((kind, control));
                }
            }
        }
    }

    // UPnP 1.0 put the base for relative URLs in URLBase; later versions use
    // the description's own URL. One that points off the gateway is ignored.
    let base = url_base
        .and_then(|text| Url::parse(&text, *location.addr.ip()))
        .unwrap_or_else(|| location.clone());
    let found: Vec<Service> = listed
        .into_iter()
        .filter_map(|(kind, control)| {
            Some(Service {
                kind,
                control: base.join(&control)?,
            })
        })
        .collect();
    if found.is_empty() {
        return Err(UpnpError::NoService(location.clone()));
    }
    Ok(found)
}

pub fn external_ip(service: &Service) -> Result<Ipv4Addr, UpnpError> {
    let action = Action::GetExternalIpAddress;
    let values = call(service, action, &[])?;
    // Routers with the WAN link down answer with an empty value or 0.0.0.0.
    values
        .get("NewExternalIPAddress")
        .and_then(|text| text.parse::<Ipv4Addr>().ok())
        .filter(|ip| !ip.is_unspecified())
        .ok_or(UpnpError::BadAnswer(action, "external address"))
}

// Maps UDP `external_port` on the router to `internal`. The entry already on
// the port is read first: the IGD spec has AddPortMapping over a forward to
// the same PC succeed and replace it, so the room would take over the user's
// own forward and delete it when it closes. A forward to us that Booth did
// not make is used as it is; a port that is someone else's, or one that
// turns out to be taken all the same, moves to one of three random ports.
pub fn add_mapping(
    service: &Service,
    internal: SocketAddrV4,
    external_port: u16,
    note: &mut dyn FnMut(fmt::Arguments<'_>),
) -> Result<Mapped, UpnpError> {
    let mut lease = LEASE;
    let free = match mapping_entry(service, external_port) {
        Ok(entry) => match entry.owner(internal) {
            Owner::Booth => {
                note(format_args!(
                    "port {external_port} already forwards to {internal} for Booth, {} s left, asking for it again",
                    entry.lease
                ));
                true
            }
            Owner::ThisPc => return Ok(kept(external_port, internal, &entry, note)),
            Owner::Other => {
                note_taken(external_port, &entry, note);
                false
            }
        },
        Err(UpnpError::Fault(_, Fault::NO_SUCH_ENTRY)) => true,
        Err(err) => {
            note(format_args!(
                "could not read the entry for port {external_port}: {err}; asking for it all the same"
            ));
            true
        }
    };

    if free {
        match add_or_permanent(service, internal, external_port, &mut lease, note) {
            Ok(()) => {
                return Ok(Mapped {
                    external_port,
                    lease,
                    delete_on_close: true,
                });
            }
            Err(UpnpError::Fault(_, Fault::CONFLICT)) => {
                note(format_args!(
                    "port {external_port} is taken on the router ({})",
                    Fault::CONFLICT
                ));
            }
            Err(err) => return Err(err),
        }
        match mapping_entry(service, external_port) {
            Ok(entry) => match entry.owner(internal) {
                // Some routers answer a conflict even for the entry the
                // request would only renew. Its lease must start again, or
                // it runs out while the room still counts on it.
                Owner::Booth => {
                    note(format_args!(
                        "the router will not renew Booth's own entry for port {external_port}, deleting it and asking again"
                    ));
                    delete_mapping(service, external_port)?;
                    add_or_permanent(service, internal, external_port, &mut lease, note)?;
                    return Ok(Mapped {
                        external_port,
                        lease,
                        delete_on_close: true,
                    });
                }
                Owner::ThisPc => return Ok(kept(external_port, internal, &entry, note)),
                Owner::Other => note_taken(external_port, &entry, note),
            },
            Err(err) => note(format_args!(
                "could not read the entry for port {external_port}: {err}"
            )),
        }
    }

    let mut tried = vec![external_port, internal.port()];
    for _ in 0..RANDOM_PORTS {
        let port = random_port(&tried);
        tried.push(port);
        note(format_args!("trying port {port} instead"));
        match add_or_permanent(service, internal, port, &mut lease, note) {
            Ok(()) => {
                return Ok(Mapped {
                    external_port: port,
                    lease,
                    delete_on_close: true,
                });
            }
            Err(UpnpError::Fault(_, Fault::CONFLICT)) => {
                note(format_args!("port {port} is taken too"));
            }
            Err(err) => return Err(err),
        }
    }
    Err(UpnpError::NoFreePort(external_port))
}

fn kept(
    external_port: u16,
    internal: SocketAddrV4,
    entry: &Entry,
    note: &mut dyn FnMut(fmt::Arguments<'_>),
) -> Mapped {
    note(format_args!(
        "port {external_port} already forwards to {internal} (\"{}\", lease {} s), using it as it is",
        entry.description, entry.lease
    ));
    Mapped {
        external_port,
        lease: entry.lease,
        delete_on_close: false,
    }
}

fn note_taken(external_port: u16, entry: &Entry, note: &mut dyn FnMut(fmt::Arguments<'_>)) {
    note(format_args!(
        "port {external_port} forwards to {}{} (\"{}\")",
        entry.internal,
        if entry.enabled { "" } else { ", disabled" },
        entry.description
    ));
}

// Older routers refuse any lease but 0. Such a mapping never expires on its
// own; the room must delete it on close, which Mapped already says.
fn add_or_permanent(
    service: &Service,
    internal: SocketAddrV4,
    external_port: u16,
    lease: &mut u32,
    note: &mut dyn FnMut(fmt::Arguments<'_>),
) -> Result<(), UpnpError> {
    match add_once(service, internal, external_port, *lease) {
        Err(UpnpError::Fault(_, Fault::ONLY_PERMANENT_LEASES)) if *lease != 0 => {
            note(format_args!(
                "the router refused a {} s lease ({}), asking again with lease 0",
                *lease,
                Fault::ONLY_PERMANENT_LEASES
            ));
            *lease = 0;
            add_once(service, internal, external_port, 0)
        }
        other => other,
    }
}

fn add_once(
    service: &Service,
    internal: SocketAddrV4,
    external_port: u16,
    lease: u32,
) -> Result<(), UpnpError> {
    let external = external_port.to_string();
    let internal_port = internal.port().to_string();
    let client = internal.ip().to_string();
    let lease = lease.to_string();
    // The order is the one in the service description; some routers read
    // the arguments by position.
    call(
        service,
        Action::AddPortMapping,
        &[
            ("NewRemoteHost", ""),
            ("NewExternalPort", &external),
            ("NewProtocol", "UDP"),
            ("NewInternalPort", &internal_port),
            ("NewInternalClient", &client),
            ("NewEnabled", "1"),
            ("NewPortMappingDescription", MAPPING_NAME),
            ("NewLeaseDuration", &lease),
        ],
    )
    .map(drop)
}

pub fn mapping_entry(service: &Service, external_port: u16) -> Result<Entry, UpnpError> {
    let action = Action::GetSpecificPortMappingEntry;
    let port = external_port.to_string();
    let values = call(
        service,
        action,
        &[
            ("NewRemoteHost", ""),
            ("NewExternalPort", &port),
            ("NewProtocol", "UDP"),
        ],
    )?;
    let internal_port = values
        .get("NewInternalPort")
        .and_then(|text| text.parse::<u16>().ok())
        .ok_or(UpnpError::BadAnswer(action, "internal port"))?;
    let client = values
        .get("NewInternalClient")
        .and_then(|text| text.parse::<Ipv4Addr>().ok())
        .ok_or(UpnpError::BadAnswer(action, "internal client"))?;
    let enabled = values
        .get("NewEnabled")
        .is_none_or(|text| !(text == "0" || text.eq_ignore_ascii_case("false")));
    let lease = match values.get("NewLeaseDuration") {
        None | Some("") => 0,
        Some(text) => text
            .parse()
            .map_err(|_| UpnpError::BadAnswer(action, "lease duration"))?,
    };
    Ok(Entry {
        internal: SocketAddrV4::new(client, internal_port),
        enabled,
        lease,
        description: printable(
            values.get("NewPortMappingDescription").unwrap_or_default(),
            64,
        ),
    })
}

pub fn delete_mapping(service: &Service, external_port: u16) -> Result<(), UpnpError> {
    let port = external_port.to_string();
    call(
        service,
        Action::DeletePortMapping,
        &[
            ("NewRemoteHost", ""),
            ("NewExternalPort", &port),
            ("NewProtocol", "UDP"),
        ],
    )
    .map(drop)
}

fn random_port(avoid: &[u16]) -> u16 {
    loop {
        let mut bytes = [0u8; 2];
        // ProcessPrng, which getrandom uses on Windows 10 and later, never fails.
        getrandom::fill(&mut bytes).expect("windows random number generator failed");
        let port = DYNAMIC_PORTS | (u16::from_be_bytes(bytes) & 0x3FFF);
        if !avoid.contains(&port) {
            return port;
        }
    }
}

// Every argument value is ours (digits, an address, "UDP", MAPPING_NAME), so
// none needs escaping.
fn call(service: &Service, action: Action, args: &[(&str, &str)]) -> Result<Values, UpnpError> {
    let urn = service.kind.urn();
    let name = action.name();
    let mut body = format!(
        "<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{name} xmlns:u=\"{urn}\">"
    );
    for (arg, value) in args {
        body.push_str(&format!("<{arg}>{value}</{arg}>"));
    }
    body.push_str(&format!("</u:{name}></s:Body></s:Envelope>\r\n"));
    // One buffer, one write: some router web servers read the body only if
    // it arrives in the same segment as the headers.
    let request = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: text/xml; charset=\"utf-8\"\r\nSOAPAction: \"{urn}#{name}\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        service.control.path,
        service.control.addr,
        body.len()
    );
    let answer = exchange(service.control.addr, request.as_bytes())?;
    soap_answer(action, answer.status, &answer.body)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Values(Vec<(String, String)>);

impl Values {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.trim())
    }
}

// The values inside <{action}Response>, or the fault. A router answers a
// refused action with HTTP 500 and a fault body, but some send the fault
// with 200. The body decides.
pub fn soap_answer(action: Action, status: u16, body: &[u8]) -> Result<Values, UpnpError> {
    if body.len() > MAX_ANSWER {
        return Err(UpnpError::TooLarge);
    }
    match scan_soap(action, body) {
        Ok((_, Some(code))) => match code.trim().parse::<u16>() {
            Ok(code) => Err(UpnpError::Fault(action, Fault(code))),
            Err(_) => Err(UpnpError::BadAnswer(action, "error code")),
        },
        _ if status != 200 => Err(UpnpError::Status(status)),
        Ok((Some(values), None)) => Ok(values),
        Ok((None, None)) => Err(UpnpError::BadAnswer(action, "response element")),
        Err(why) => Err(UpnpError::BadXml(why)),
    }
}

type Scanned = (Option<Values>, Option<String>);

fn scan_soap(action: Action, body: &[u8]) -> Result<Scanned, &'static str> {
    let response = format!("{}Response", action.name());
    let mut doc = Doc::new(body);
    let mut response_depth = None;
    let mut seen_response = false;
    // The name is None for the fault's errorCode.
    let mut value: Option<(Option<&[u8]>, String)> = None;
    let mut values = Vec::new();
    let mut error_code = None;

    while let Some(event) = doc.next_event()? {
        match event {
            Event::Open(name) => {
                if value.is_some() {
                    return Err("an element inside a value");
                }
                if !seen_response && is(name, &response) {
                    seen_response = true;
                    response_depth = Some(doc.depth());
                } else if response_depth.is_some_and(|depth| doc.depth() == depth + 1) {
                    value = Some((Some(name), String::new()));
                } else if is(name, "errorCode") {
                    value = Some((None, String::new()));
                }
            }
            Event::Text(raw) => {
                if let Some((_, text)) = &mut value {
                    decode(raw, text)?;
                }
            }
            Event::Cdata(raw) => {
                if let Some((_, text)) = &mut value {
                    append_cdata(raw, text)?;
                }
            }
            Event::Close(_) => match value.take() {
                Some((Some(name), text)) => {
                    if values.len() < MAX_VALUES {
                        values.push((String::from_utf8_lossy(name).into_owned(), text));
                    }
                }
                Some((None, text)) => {
                    error_code.get_or_insert(text);
                }
                None => {
                    if response_depth.is_some_and(|depth| doc.depth() < depth) {
                        response_depth = None;
                    }
                }
            },
        }
    }
    Ok((seen_response.then_some(Values(values)), error_code))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Http {
    pub status: u16,
    pub body: Vec<u8>,
}

// None while more bytes could still complete the answer. `closed` says the
// router has closed the connection, which ends a body without a length.
pub fn http_response(buf: &[u8], closed: bool) -> Result<Option<Http>, UpnpError> {
    if buf.len() > MAX_ANSWER {
        return Err(UpnpError::TooLarge);
    }
    let early = |what| {
        if closed {
            Err(UpnpError::BadHttp(what))
        } else {
            Ok(None)
        }
    };
    let Some(body_at) = head_end(buf) else {
        return early("the connection closed inside the headers");
    };
    let head = buf
        .get(..body_at)
        .and_then(|head| str::from_utf8(head).ok())
        .ok_or(UpnpError::BadHttp("the headers are not text"))?;
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(status_code)
        .ok_or(UpnpError::BadHttp(
            "the status line is not HTTP/1.x and a code",
        ))?;

    let mut length: Option<usize> = None;
    let mut chunked = false;
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or(UpnpError::BadHttp("a header line has no colon"))?;
        let (name, value) = (name.trim(), value.trim());
        if name.eq_ignore_ascii_case("content-length") {
            let parsed = value
                .parse::<usize>()
                .map_err(|_| UpnpError::BadHttp("the content length is not a number"))?;
            if length.is_some_and(|known| known != parsed) {
                return Err(UpnpError::BadHttp("two different content lengths"));
            }
            length = Some(parsed);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            if value.eq_ignore_ascii_case("chunked") {
                chunked = true;
            } else if !value.eq_ignore_ascii_case("identity") {
                return Err(UpnpError::BadHttp("an unsupported transfer encoding"));
            }
        }
    }

    let rest = buf.get(body_at..).unwrap_or_default();
    // Chunked wins over a length when both are sent, as HTTP/1.1 says.
    let body = if chunked {
        match dechunk(rest)? {
            Some(body) => body,
            None => return early("the connection closed inside a chunk"),
        }
    } else if let Some(length) = length {
        if length > MAX_ANSWER {
            return Err(UpnpError::TooLarge);
        }
        match rest.get(..length) {
            Some(body) => body.to_vec(),
            None => return early("the connection closed before the whole body arrived"),
        }
    } else if closed {
        rest.to_vec()
    } else {
        return Ok(None);
    };
    Ok(Some(Http { status, body }))
}

// Embedded web servers sometimes end lines with a bare LF, so both are taken.
fn head_end(buf: &[u8]) -> Option<usize> {
    buf.iter()
        .enumerate()
        .filter(|(_, b)| **b == b'\n')
        .find_map(|(at, _)| match buf.get(at + 1..) {
            Some([b'\n', ..]) => Some(at + 2),
            Some([b'\r', b'\n', ..]) => Some(at + 3),
            _ => None,
        })
}

fn status_code(line: &str) -> Option<u16> {
    let mut words = line.split_ascii_whitespace();
    if !words.next()?.starts_with("HTTP/1.") {
        return None;
    }
    let code = words.next()?;
    if code.len() != 3 {
        return None;
    }
    code.parse().ok().filter(|code| (100..=999).contains(code))
}

fn dechunk(mut rest: &[u8]) -> Result<Option<Vec<u8>>, UpnpError> {
    let mut body = Vec::new();
    loop {
        let Some((line, after)) = split_line(rest) else {
            return Ok(None);
        };
        let size = line
            .split(|b| *b == b';')
            .next()
            .and_then(|text| str::from_utf8(text).ok())
            .map(str::trim)
            .filter(|text| !text.is_empty() && text.len() <= 8)
            .and_then(|text| usize::from_str_radix(text, 16).ok())
            .ok_or(UpnpError::BadHttp("a chunk size is not a hex number"))?;
        rest = after;
        if size == 0 {
            // Trailer lines up to an empty one; nothing in them is used.
            loop {
                let Some((line, after)) = split_line(rest) else {
                    return Ok(None);
                };
                rest = after;
                if line.is_empty() {
                    return Ok(Some(body));
                }
            }
        }
        if body.len().saturating_add(size) > MAX_ANSWER {
            return Err(UpnpError::TooLarge);
        }
        let Some(data) = rest.get(..size) else {
            return Ok(None);
        };
        body.extend_from_slice(data);
        rest = match rest.get(size..).unwrap_or_default() {
            [b'\r', b'\n', after @ ..] | [b'\n', after @ ..] => after,
            [] | [b'\r'] => return Ok(None),
            _ => return Err(UpnpError::BadHttp("a chunk does not end with a line break")),
        };
    }
}

fn split_line(buf: &[u8]) -> Option<(&[u8], &[u8])> {
    let at = buf.iter().position(|b| *b == b'\n')?;
    let line = buf.get(..at)?;
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    Some((line, buf.get(at + 1..)?))
}

// One request on its own connection, all of it within HTTP_TIMEOUT, so a
// router that trickles bytes cannot hold the mapper thread longer than a
// silent one.
fn exchange(addr: SocketAddrV4, request: &[u8]) -> Result<Http, UpnpError> {
    let deadline = Instant::now() + HTTP_TIMEOUT;
    let failed = move |err: io::Error| match err.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => UpnpError::Timeout(addr),
        _ => UpnpError::Io(addr, err),
    };
    let time_left = || {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            Err(UpnpError::Timeout(addr))
        } else {
            Ok(left)
        }
    };

    let mut stream = match TcpStream::connect_timeout(&SocketAddr::V4(addr), HTTP_TIMEOUT) {
        Ok(stream) => stream,
        Err(err) if err.kind() == io::ErrorKind::TimedOut => {
            return Err(UpnpError::Timeout(addr));
        }
        Err(err) => return Err(UpnpError::Connect(addr, err)),
    };
    stream
        .set_write_timeout(Some(time_left()?))
        .map_err(failed)?;
    stream.write_all(request).map_err(failed)?;

    let mut answer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        stream
            .set_read_timeout(Some(time_left()?))
            .map_err(failed)?;
        match stream.read(&mut chunk) {
            Ok(0) => {
                return http_response(&answer, true)?.ok_or(UpnpError::BadHttp(
                    "the connection closed before the answer was complete",
                ));
            }
            Ok(len) => {
                if answer.len() + len > MAX_ANSWER {
                    return Err(UpnpError::TooLarge);
                }
                answer.extend_from_slice(chunk.get(..len).unwrap_or_default());
                if let Some(done) = http_response(&answer, false)? {
                    return Ok(done);
                }
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(failed(err)),
        }
    }
}

enum Event<'a> {
    Open(&'a [u8]),
    Close(&'a [u8]),
    Text(&'a [u8]),
    Cdata(&'a [u8]),
}

// Just enough XML for router descriptions and SOAP answers: elements by
// local name (any namespace prefix dropped), text, CDATA, comments and the
// XML declaration. Attributes are stepped over. A DOCTYPE, a tag that does
// not close, or a closing tag that does not match is an error rather than a
// guess.
struct Doc<'a> {
    rest: &'a [u8],
    open: Vec<&'a [u8]>,
    // A self-closing tag comes out as its open, then this close.
    closing: Option<&'a [u8]>,
}

impl<'a> Doc<'a> {
    fn new(xml: &'a [u8]) -> Doc<'a> {
        Doc {
            rest: xml,
            open: Vec::new(),
            closing: None,
        }
    }

    fn depth(&self) -> usize {
        self.open.len()
    }

    fn parent(&self) -> Option<&'a [u8]> {
        let at = self.open.len().checked_sub(2)?;
        self.open.get(at).copied()
    }

    fn next_event(&mut self) -> Result<Option<Event<'a>>, &'static str> {
        let event = self.step();
        if event.is_err() {
            self.rest = &[];
        }
        event
    }

    fn step(&mut self) -> Result<Option<Event<'a>>, &'static str> {
        if let Some(name) = self.closing.take() {
            self.open.pop();
            return Ok(Some(Event::Close(name)));
        }
        loop {
            let rest = self.rest;
            let Some(&first) = rest.first() else {
                if self.open.is_empty() {
                    return Ok(None);
                }
                return Err("the document ends inside an element");
            };
            if first != b'<' {
                let end = rest.iter().position(|b| *b == b'<').unwrap_or(rest.len());
                let (text, after) = rest.split_at(end);
                self.rest = after;
                return Ok(Some(Event::Text(text)));
            }
            if let Some(after) = rest.strip_prefix(b"<!--") {
                self.rest = skip_past(after, b"-->").ok_or("a comment never ends")?;
                continue;
            }
            if let Some(after) = rest.strip_prefix(b"<![CDATA[") {
                let end = find_bytes(after, b"]]>").ok_or("a CDATA section never ends")?;
                self.rest = after.get(end + 3..).unwrap_or_default();
                return Ok(Some(Event::Cdata(after.get(..end).unwrap_or_default())));
            }
            if let Some(after) = rest.strip_prefix(b"<?") {
                self.rest = skip_past(after, b"?>").ok_or("a processing instruction never ends")?;
                continue;
            }
            // A DOCTYPE can define entities and pull in other files; refusing
            // it is simpler and safer than following it.
            if rest.starts_with(b"<!") {
                return Err("the document has a DOCTYPE or another declaration");
            }
            if let Some(after) = rest.strip_prefix(b"</") {
                let end = after
                    .iter()
                    .position(|b| *b == b'>')
                    .ok_or("a closing tag never ends")?;
                let name = local_name(after.get(..end).unwrap_or_default().trim_ascii());
                self.rest = after.get(end + 1..).unwrap_or_default();
                let open = self.open.pop().ok_or("a closing tag with nothing open")?;
                if !open.eq_ignore_ascii_case(name) {
                    return Err("a closing tag does not match the open one");
                }
                return Ok(Some(Event::Close(name)));
            }

            let after = rest.get(1..).unwrap_or_default();
            let end = tag_end(after).ok_or("a tag is not closed")?;
            let inner = after.get(..end).unwrap_or_default().trim_ascii_end();
            self.rest = after.get(end + 1..).unwrap_or_default();
            let (inner, empty) = match inner.strip_suffix(b"/") {
                Some(inner) => (inner, true),
                None => (inner, false),
            };
            let name_len = inner
                .iter()
                .position(|b| b.is_ascii_whitespace())
                .unwrap_or(inner.len());
            let name = inner.get(..name_len).unwrap_or_default();
            if name.is_empty() || !name.iter().all(|b| is_name_byte(*b)) {
                return Err("a tag has no valid name");
            }
            if self.open.len() >= MAX_DEPTH {
                return Err("elements are nested too deep");
            }
            let name = local_name(name);
            self.open.push(name);
            if empty {
                self.closing = Some(name);
            }
            return Ok(Some(Event::Open(name)));
        }
    }
}

// The end of a tag, stepping over quoted attribute values, which may hold '>'.
fn tag_end(buf: &[u8]) -> Option<usize> {
    let mut quote = None;
    for (at, &b) in buf.iter().enumerate() {
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if b == b'"' || b == b'\'' => quote = Some(b),
            None if b == b'>' => return Some(at),
            None if b == b'<' => return None,
            None => {}
        }
    }
    None
}

fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':') || b >= 0x80
}

fn local_name(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|b| *b == b':') {
        Some(at) => name.get(at + 1..).unwrap_or_default(),
        None => name,
    }
}

fn is(name: &[u8], wanted: &str) -> bool {
    name.eq_ignore_ascii_case(wanted.as_bytes())
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn skip_past<'a>(haystack: &'a [u8], needle: &[u8]) -> Option<&'a [u8]> {
    let at = find_bytes(haystack, needle)?;
    haystack.get(at + needle.len()..)
}

fn decode(raw: &[u8], out: &mut String) -> Result<(), &'static str> {
    let mut rest = str::from_utf8(raw).map_err(|_| "a value is not utf-8")?;
    while let Some(at) = rest.find('&') {
        out.push_str(rest.get(..at).unwrap_or_default());
        let after = rest.get(at + 1..).unwrap_or_default();
        let end = after
            .find(';')
            .filter(|end| *end <= 10)
            .ok_or("an entity is not closed")?;
        let c = match after.get(..end).unwrap_or_default() {
            "lt" => '<',
            "gt" => '>',
            "amp" => '&',
            "quot" => '"',
            "apos" => '\'',
            other => char_reference(other).ok_or("an unknown entity")?,
        };
        out.push(c);
        rest = after.get(end + 1..).unwrap_or_default();
    }
    out.push_str(rest);
    if out.len() > MAX_VALUE {
        return Err("a value is longer than 1024 bytes");
    }
    Ok(())
}

fn char_reference(entity: &str) -> Option<char> {
    let number = entity.strip_prefix('#')?;
    let code = match number.strip_prefix(['x', 'X']) {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => number.parse().ok()?,
    };
    char::from_u32(code)
}

fn append_cdata(raw: &[u8], out: &mut String) -> Result<(), &'static str> {
    out.push_str(str::from_utf8(raw).map_err(|_| "a value is not utf-8")?);
    if out.len() > MAX_VALUE {
        return Err("a value is longer than 1024 bytes");
    }
    Ok(())
}

// Router text headed for the log: printable ASCII only, so a hostile answer
// cannot start a new log line or hide in control characters.
fn printable(text: &str, max: usize) -> String {
    text.chars()
        .take(max)
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .collect()
}
