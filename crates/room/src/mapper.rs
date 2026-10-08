// Asks the router to open the room's port, on a thread of its own because
// UPnP is plain blocking HTTP with up to 2 s per call. PCP first, then
// NAT-PMP, then UPnP; the first that works is kept and renewed at half its
// lifetime. What comes of it goes to the timer thread as a Report. Nothing
// here touches the room's state or its socket.

use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Select, Sender, TryRecvError};
use net::addrs::Gateway;
use net::natpmp::{self, NatPmpError};
use net::pcp::{self, PcpError, ResultCode};
use net::upnp::{self, Fault, Owner, UpnpError};

use crate::log::{Log, Writer, log, secs};

#[cfg(test)]
mod tests;

// Two hours, renewed at half, so one missed renewal does not drop it.
const LIFETIME: u32 = 7200;
// RFC 6887 asks PCP servers for 120 s at least. A router, or a spoofed
// NAT-PMP answer, that grants a few seconds would have the mapper renew
// every second for the life of the room and push the room out of its log.
const LEAST_LIFETIME: u32 = 120;
const TRIES: [Duration; 2] = [Duration::from_millis(250), Duration::from_millis(500)];
const SSDP_WAIT: Duration = Duration::from_secs(1);
// A deletion is a few seconds at worst, so a thread still going after this
// is stuck, and the new room asks the router all the same.
const EARLIER_WAIT: Duration = Duration::from_secs(10);
// A router that just came back can take a while to answer mapping asks
// again: OpenWrt restarts miniupnpd when the WAN comes up, at the moment
// STUN shows the new address. A mapping lost then is asked for again after
// each of these, 5, 15 and 60 s after the loss, and on every later ask to
// map again.
const AGAIN: [Duration; 3] = [
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(45),
];

// Mapper threads still at work when their room let go of them, by port. A
// new room on the same port waits for them, or an old thread's deletion
// would remove the new room's mapping. The process waits for them too
// before it exits, or Windows ends them halfway through a deletion and the
// mapping stays on the router.
static FINISHING: Mutex<Vec<(u16, Receiver<()>)>> = Mutex::new(Vec::new());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Protocol {
    Pcp,
    NatPmp,
    Upnp,
}

impl Protocol {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Protocol::Pcp => "PCP",
            Protocol::NatPmp => "NAT-PMP",
            Protocol::Upnp => "UPnP",
        }
    }

    // What every log line about it starts with.
    fn word(self) -> &'static str {
        match self {
            Protocol::Pcp => "pcp",
            Protocol::NatPmp => "nat-pmp",
            Protocol::Upnp => "upnp",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Report {
    // The first mapping, a renewal, or a new mapping after a renewal failed.
    // A lifetime of 0 is a UPnP mapping that never runs out.
    Mapped {
        protocol: Protocol,
        external: SocketAddrV4,
        lifetime: u32,
    },
    // Nothing mapped, on the first try or after a failed renewal and one more
    // try. `wan` is an outside address a router reported all the same, which
    // is what the second router check needs.
    Unmapped {
        wan: Option<(Protocol, Ipv4Addr)>,
    },
}

// Where the questions go: in real use the gateway on PCP's port and the SSDP
// multicast group, in tests fakes on loopback.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Target {
    pub gateway: Gateway,
    pub pcp: SocketAddr,
    pub ssdp: SocketAddr,
    // PCP and NAT-PMP grants shorter than this are not used, and no renewal
    // comes sooner than half of it. Tests on loopback lower it.
    pub least_lifetime: u32,
    // AGAIN, which tests shorten.
    pub again: [Duration; 3],
}

impl Target {
    pub(crate) fn router(gateway: Gateway) -> Target {
        Target {
            gateway,
            pcp: SocketAddr::from((gateway.ip, pcp::PORT)),
            ssdp: upnp::SSDP,
            least_lifetime: LEAST_LIFETIME,
            again: AGAIN,
        }
    }

    // After a router restart this PC can have another address, or be behind
    // another router. Tests keep their fake's port.
    fn moved_to(&mut self, gateway: Gateway) {
        self.gateway = gateway;
        self.pcp.set_ip(IpAddr::V4(gateway.ip));
    }
}

pub(crate) struct Mapper {
    port: u16,
    close: Sender<()>,
    renew: Sender<()>,
    remap: Sender<Gateway>,
    // Nothing is sent on it. It disconnects when the thread ends, which a
    // join cannot wait for with a time limit.
    done: Receiver<()>,
    thread: Option<JoinHandle<()>>,
    // The log writer, handed over when the thread outlives leave(), so its
    // lines about the deletion still reach the file.
    writer: Arc<Mutex<Option<Writer>>>,
}

impl Mapper {
    pub(crate) fn start(
        target: Target,
        port: u16,
        log: Log,
        report: impl FnMut(Report) + Send + 'static,
    ) -> io::Result<Mapper> {
        let (close, closing) = crossbeam_channel::bounded(1);
        let (renew, asked) = crossbeam_channel::bounded(1);
        let (remap, remap_asked) = crossbeam_channel::bounded(1);
        let (finished, done) = crossbeam_channel::bounded::<()>(0);
        let writer: Arc<Mutex<Option<Writer>>> = Arc::default();
        let mut job = Job {
            target,
            port,
            nonce: pcp::new_nonce(),
            log,
            report: Box::new(report),
            closing,
            asked,
            remap_asked,
            earlier: finishing_on(port),
            closed: false,
            stale: Vec::new(),
        };
        let thread = thread::Builder::new()
            .name("room port mapping".into())
            .spawn({
                let writer = Arc::clone(&writer);
                move || {
                    job.run();
                    // Taken out first: stopping the writer waits for it, and
                    // hand_over must not wait behind that.
                    let taken = lock(&writer).take();
                    drop(taken);
                    drop(finished);
                }
            })?;
        Ok(Mapper {
            port,
            close,
            renew,
            remap,
            done,
            thread: Some(thread),
            writer,
        })
    }

    // The thread deletes the mapping and ends. Whatever it is doing now, a
    // step of the ladder or a renewal, it finishes first.
    pub(crate) fn close(&self) {
        let _ = self.close.try_send(());
    }

    // For the room to ask for a renewal now, when STUN sees the outside
    // address change: a renewal asks the router for its address too. One
    // ask waiting is enough, so a second is dropped.
    pub(crate) fn renewer(&self) -> Sender<()> {
        self.renew.clone()
    }

    // For the room to have the mapping deleted and asked for again, when
    // this PC's outside address really changed: some routers keep listing a
    // mapping that stopped working, and renewing that one does not help.
    pub(crate) fn remapper(&self) -> Sender<Gateway> {
        self.remap.clone()
    }

    // False when the thread is still at it by `deadline`; it then finishes
    // on its own.
    pub(crate) fn wait(&mut self, deadline: Instant) -> bool {
        match self.done.recv_deadline(deadline) {
            Err(RecvTimeoutError::Disconnected) => {
                if let Some(thread) = self.thread.take() {
                    let _ = thread.join();
                }
                true
            }
            Ok(()) | Err(RecvTimeoutError::Timeout) => false,
        }
    }

    // If the thread has ended since wait() gave up, the writer stops when
    // this Mapper is dropped instead.
    pub(crate) fn hand_over(&self, writer: Writer) {
        *lock(&self.writer) = Some(writer);
    }
}

impl Drop for Mapper {
    fn drop(&mut self) {
        self.close();
        if self.thread.is_some() && !ended(&self.done) {
            lock(&FINISHING).push((self.port, self.done.clone()));
        }
    }
}

// True when every mapper thread let go of while still at work has ended by
// `within` from now.
pub(crate) fn finish(within: Duration) -> bool {
    let deadline = Instant::now() + within;
    let left: Vec<Receiver<()>> = {
        let mut finishing = lock(&FINISHING);
        finishing.retain(|(_, done)| !ended(done));
        finishing.iter().map(|(_, done)| done.clone()).collect()
    };
    left.iter().all(|done| {
        matches!(
            done.recv_deadline(deadline),
            Err(RecvTimeoutError::Disconnected)
        )
    })
}

fn finishing_on(port: u16) -> Vec<Receiver<()>> {
    let mut finishing = lock(&FINISHING);
    finishing.retain(|(_, done)| !ended(done));
    finishing
        .iter()
        .filter(|(on, _)| *on == port)
        .map(|(_, done)| done.clone())
        .collect()
}

fn ended(done: &Receiver<()>) -> bool {
    matches!(done.try_recv(), Err(TryRecvError::Disconnected))
}

enum Held {
    Pcp(pcp::Client),
    NatPmp(natpmp::Client),
    Upnp(UpnpHeld),
}

#[derive(Clone)]
struct UpnpHeld {
    service: upnp::Service,
    internal: SocketAddrV4,
    external_port: u16,
    lease: u32,
    // False for a forward the router already had to this PC that Booth did
    // not make, most likely the user's own. It is not the room's to remove.
    delete_on_close: bool,
}

impl Held {
    fn protocol(&self) -> Protocol {
        match self {
            Held::Pcp(_) => Protocol::Pcp,
            Held::NatPmp(_) => Protocol::NatPmp,
            Held::Upnp(_) => Protocol::Upnp,
        }
    }

    // PCP and NAT-PMP know a mapping by this PC's address and internal port
    // (and PCP by the nonce, which stays the same), so asking again made the
    // same one.
    fn same_as(&self, other: &Held) -> bool {
        match (self, other) {
            (Held::Pcp(_), Held::Pcp(_)) | (Held::NatPmp(_), Held::NatPmp(_)) => true,
            (Held::Upnp(a), Held::Upnp(b)) => a.external_port == b.external_port,
            _ => false,
        }
    }

    // The same mapping, asked for again after a renewal failed. What the room
    // made stays the room's to delete, also on a router that shows it under
    // no name and so as someone else's.
    fn take_over(&mut self, old: &Held) {
        if let (Held::Upnp(new), Held::Upnp(old)) = (self, old) {
            new.delete_on_close |= old.delete_on_close;
        }
    }
}

struct Holding {
    held: Held,
    external: SocketAddrV4,
    lifetime: u32,
    renew_at: Option<Instant>,
}

impl Holding {
    fn new(held: Held, external: SocketAddrV4, lifetime: u32, least: u32) -> Holding {
        Holding {
            held,
            external,
            lifetime,
            renew_at: renew_at(lifetime, least),
        }
    }

    fn report(&self) -> Report {
        Report::Mapped {
            protocol: self.held.protocol(),
            external: self.external,
            lifetime: self.lifetime,
        }
    }
}

// A UPnP forward that was already there can have any lease left, so the
// floor matters for it too.
fn renew_at(lifetime: u32, least: u32) -> Option<Instant> {
    // A lease of 0 never runs out, but a router that restarts forgets it
    // all the same, and only a renewal finds that out.
    let lifetime = if lifetime == 0 { LIFETIME } else { lifetime };
    let half = Duration::from_secs(u64::from(lifetime.max(least) / 2));
    Some(Instant::now() + half)
}

type Wan = Option<(Protocol, Ipv4Addr)>;

// Where the ladder goes after PCP made no mapping.
enum Next {
    NatPmp,
    // NAT-PMP is left out, for this reason.
    Upnp(&'static str),
}

enum Woke {
    Closing,
    Asked,
    Remap(Gateway),
    // The room dropped its end of the renewal asks.
    LetGo,
    // Or of the asks to map again.
    RemapLetGo,
}

// What ended the wait for the next renewal.
enum Wake {
    // The renewal is due, or the room asked for it now.
    Renew,
    // Map again, at this router.
    Remap(Gateway),
    Closing,
}

struct Job {
    target: Target,
    port: u16,
    // One for the whole room: the router refuses a PCP renewal or deletion
    // that does not carry the nonce which made the mapping.
    nonce: [u8; 12],
    log: Log,
    report: Box<dyn FnMut(Report) + Send>,
    closing: Receiver<()>,
    // The room asks for a renewal now.
    asked: Receiver<()>,
    // The room asks for the mapping to be deleted and made again.
    remap_asked: Receiver<Gateway>,
    // The threads of earlier rooms on this port that were still at work.
    earlier: Vec<Receiver<()>>,
    closed: bool,
    // Mappings that stopped renewing but may still be on the router. They
    // are deleted at close with the current one.
    stale: Vec<Held>,
}

impl Job {
    fn run(&mut self) {
        if !self.wait_for_earlier() {
            log!(
                self.log,
                "port mapping: room closed before the router was asked, nothing to delete"
            );
            return;
        }
        let mut current = self.ladder_and_report();
        while let Some(holding) = current.take() {
            current = match self.wait(holding.renew_at) {
                Wake::Renew => self.renew(holding),
                Wake::Remap(gateway) => self.remap(holding, gateway),
                Wake::Closing => {
                    current = Some(holding);
                    break;
                }
            };
            if current.is_none() {
                current = self.map_again();
            }
        }
        // Nothing mapped now, but something from before a failed renewal may
        // still be on the router until its lifetime runs out.
        if current.is_none() && !self.stale.is_empty() {
            self.wait_for_close();
        }
        match current {
            Some(holding) => {
                log!(
                    self.log,
                    "port mapping: room closed, deleting the {} mapping for {}",
                    holding.held.protocol().word(),
                    holding.external
                );
                self.delete(holding.held);
            }
            None if self.closed && self.stale.is_empty() => {
                log!(self.log, "port mapping: room closed, nothing to delete");
            }
            None => {}
        }
        for held in std::mem::take(&mut self.stale) {
            log!(
                self.log,
                "port mapping: room closed, deleting an older {} mapping",
                held.protocol().word()
            );
            self.delete(held);
        }
    }

    // A thread from an earlier room still deleting on this port would
    // delete this room's mapping just after the router made it. False when
    // this room closes first.
    fn wait_for_earlier(&mut self) -> bool {
        let earlier = std::mem::take(&mut self.earlier);
        if earlier.is_empty() {
            return true;
        }
        log!(
            self.log,
            "port mapping: the last room on udp {} is still deleting its mapping, waiting for it first",
            self.port
        );
        let deadline = Instant::now() + EARLIER_WAIT;
        for done in &earlier {
            let mut select = Select::new();
            let finished = select.recv(done);
            select.recv(&self.closing);
            match select.select_deadline(deadline) {
                Ok(op) if op.index() == finished => {
                    let _ = op.recv(done);
                }
                Ok(op) => {
                    let _ = op.recv(&self.closing);
                    self.closed = true;
                    return false;
                }
                Err(_) => {
                    log!(
                        self.log,
                        "port mapping: the last room's thread is still at it after {} s, asking the router all the same",
                        EARLIER_WAIT.as_secs()
                    );
                    return true;
                }
            }
        }
        log!(self.log, "port mapping: the last room's thread is done");
        true
    }

    fn wait(&mut self, at: Option<Instant>) -> Wake {
        loop {
            if self.closing() {
                return Wake::Closing;
            }
            let woke = {
                let mut select = Select::new();
                let closing = select.recv(&self.closing);
                let asked = select.recv(&self.asked);
                select.recv(&self.remap_asked);
                let op = match at {
                    Some(at) => match select.select_deadline(at) {
                        Ok(op) => op,
                        Err(_) => return Wake::Renew,
                    },
                    None => select.select(),
                };
                if op.index() == closing {
                    let _ = op.recv(&self.closing);
                    Woke::Closing
                } else if op.index() == asked {
                    match op.recv(&self.asked) {
                        Ok(()) => Woke::Asked,
                        Err(_) => Woke::LetGo,
                    }
                } else {
                    match op.recv(&self.remap_asked) {
                        Ok(gateway) => Woke::Remap(gateway),
                        Err(_) => Woke::RemapLetGo,
                    }
                }
            };
            match woke {
                Woke::Closing => {
                    self.closed = true;
                    return Wake::Closing;
                }
                Woke::Asked => {
                    log!(
                        self.log,
                        "port mapping: stun saw the outside address change, renewing now"
                    );
                    return Wake::Renew;
                }
                Woke::Remap(gateway) => {
                    // Mapping again asks the router everything a renewal
                    // would, so one asked for meanwhile is done with it.
                    let _ = self.asked.try_recv();
                    return Wake::Remap(gateway);
                }
                Woke::LetGo => self.asked = crossbeam_channel::never(),
                Woke::RemapLetGo => self.remap_asked = crossbeam_channel::never(),
            }
        }
    }

    fn wait_for_close(&mut self) {
        if !self.closed {
            let _ = self.closing.recv();
            self.closed = true;
        }
    }

    fn closing(&mut self) -> bool {
        if !self.closed && !matches!(self.closing.try_recv(), Err(TryRecvError::Empty)) {
            self.closed = true;
        }
        self.closed
    }

    fn ladder_and_report(&mut self) -> Option<Holding> {
        match self.ladder() {
            Ok(holding) => {
                (self.report)(holding.report());
                Some(holding)
            }
            Err(wan) => {
                // A ladder the room's closing cut short has said which steps
                // it left out.
                if !self.closed {
                    log!(self.log, "port mapping: the router opened no port");
                }
                (self.report)(Report::Unmapped { wan });
                None
            }
        }
    }

    fn ladder(&mut self) -> Result<Holding, Wan> {
        let Gateway { ip, local } = self.target.gateway;
        log!(
            self.log,
            "port mapping: asking the router at {ip} to open udp {} for {local}, pcp first",
            self.port
        );
        let mut wan = None;
        match self.pcp() {
            Ok(holding) => return Ok(holding),
            Err(Next::NatPmp) => {
                if self.closing() {
                    log!(
                        self.log,
                        "port mapping: room closed before nat-pmp and upnp were asked"
                    );
                    return Err(wan);
                }
                if let Some(holding) = self.natpmp(&mut wan) {
                    return Ok(holding);
                }
            }
            Err(Next::Upnp(why)) => log!(self.log, "nat-pmp: not asked, {why}"),
        }
        if self.closing() {
            log!(self.log, "port mapping: room closed before upnp was asked");
            return Err(wan);
        }
        self.upnp(&mut wan)
    }

    fn pcp(&mut self) -> Result<Holding, Next> {
        let log = self.log.clone();
        let note = &mut |line: fmt::Arguments<'_>| log!(log, "pcp: {line}");
        let (port, local, nonce) = (self.port, self.target.gateway.local, self.nonce);
        let least = self.target.least_lifetime;
        let made = pcp::Client::new(self.target.pcp, local, port, nonce)
            .map_err(|err| PcpError::Io(err.to_string()))
            .and_then(|mut client| {
                let mapping = client.map(port, LIFETIME, &TRIES, note)?;
                Ok((client, mapping))
            });
        match made {
            Ok((_, mapping)) if mapping.lifetime < least => {
                log!(
                    self.log,
                    "pcp: the router opened {mapping}, less than the {least} s the room takes; not used, it runs out on its own"
                );
                Err(Next::Upnp("the router answered pcp"))
            }
            Ok((client, mapping)) => {
                log!(self.log, "pcp: the router opened {mapping}");
                Ok(Holding::new(
                    Held::Pcp(client),
                    mapping.external,
                    mapping.lifetime,
                    least,
                ))
            }
            Err(err) => {
                log!(self.log, "pcp: {err}");
                Err(match err {
                    // What a router says to another nonce for a port it
                    // already maps (RFC 6887 section 11.3): a run that ended
                    // without deleting its mapping, for up to two hours.
                    // NAT-PMP has no nonce.
                    PcpError::Refused {
                        code: ResultCode::NOT_AUTHORIZED,
                        ..
                    } => Next::NatPmp,
                    // NAT-PMP listens on the same port. A router that refused
                    // in PCP has already said what it thinks.
                    PcpError::Refused { .. } => Next::Upnp("the router answered pcp"),
                    PcpError::PortClosed => Next::Upnp("nothing listens on its port"),
                    _ => Next::NatPmp,
                })
            }
        }
    }

    fn natpmp(&mut self, wan: &mut Wan) -> Option<Holding> {
        let log = self.log.clone();
        let note = &mut |line: fmt::Arguments<'_>| log!(log, "nat-pmp: {line}");
        let least = self.target.least_lifetime;
        let mut client =
            match natpmp::Client::new(self.target.pcp, self.target.gateway.local, self.port) {
                Ok(client) => client,
                Err(err) => {
                    log!(self.log, "nat-pmp: {err}");
                    return None;
                }
            };
        match client.map(self.port, LIFETIME, &TRIES, note) {
            Ok(mapping) => {
                *wan = Some((Protocol::NatPmp, *mapping.external.ip()));
                if mapping.lifetime < least {
                    log!(
                        self.log,
                        "nat-pmp: the router opened {mapping}, less than the {least} s the room takes; not used, it runs out on its own"
                    );
                    return None;
                }
                log!(self.log, "nat-pmp: the router opened {mapping}");
                Some(Holding::new(
                    Held::NatPmp(client),
                    mapping.external,
                    mapping.lifetime,
                    least,
                ))
            }
            Err(err) => {
                log!(self.log, "nat-pmp: {err}");
                // The map answer carries no address, and a refusal may have
                // come after the address was given. It is asked for once
                // more, for the second router check.
                if let NatPmpError::Refused(_) = err
                    && let Ok(ip) = client.external_address(&TRIES[..1], note)
                {
                    log!(self.log, "nat-pmp: the router's outside address is {ip}");
                    *wan = Some((Protocol::NatPmp, ip));
                }
                None
            }
        }
    }

    fn upnp(&mut self, wan: &mut Wan) -> Result<Holding, Wan> {
        let log = self.log.clone();
        let note = &mut |line: fmt::Arguments<'_>| log!(log, "upnp: {line}");
        let Gateway { ip, local } = self.target.gateway;
        let router = match upnp::find(ip, local, self.target.ssdp, SSDP_WAIT, note) {
            Ok(router) => router,
            Err(err) => {
                log!(self.log, "upnp: {err}");
                return Err(*wan);
            }
        };
        *wan = Some((Protocol::Upnp, router.external_ip));
        if self.closing() {
            log!(
                self.log,
                "upnp: room closed before the mapping was asked for"
            );
            return Err(*wan);
        }
        let internal = SocketAddrV4::new(local, self.port);
        log!(
            self.log,
            "upnp: asking {} ({}) to map udp {} to {internal} for {} s",
            router.service.control,
            router.service.kind,
            self.port,
            upnp::LEASE
        );
        match upnp::add_mapping(&router.service, internal, self.port, note) {
            Ok(mapped) => {
                let external = SocketAddrV4::new(router.external_ip, mapped.external_port);
                let kept = if mapped.delete_on_close {
                    ""
                } else {
                    ", a forward the router already had, left in place when the room closes"
                };
                log!(
                    self.log,
                    "upnp: the router opened {external} for {} s{kept}",
                    mapped.lease
                );
                let held = Held::Upnp(UpnpHeld {
                    service: router.service,
                    internal,
                    external_port: mapped.external_port,
                    lease: mapped.lease,
                    delete_on_close: mapped.delete_on_close,
                });
                Ok(Holding::new(
                    held,
                    external,
                    mapped.lease,
                    self.target.least_lifetime,
                ))
            }
            Err(err) => {
                log!(self.log, "upnp: {err}");
                Err(*wan)
            }
        }
    }

    fn renew(&mut self, mut holding: Holding) -> Option<Holding> {
        let word = holding.held.protocol().word();
        let log = self.log.clone();
        let note = &mut |line: fmt::Arguments<'_>| log!(log, "{word}: {line}");
        let least = self.target.least_lifetime;
        let long_enough = |mapping: pcp::Mapping| {
            if mapping.lifetime < least {
                Err(format!(
                    "the router renewed it for {} s, less than the {least} s the room takes",
                    mapping.lifetime
                ))
            } else {
                Ok((mapping.external, mapping.lifetime))
            }
        };
        log!(self.log, "{word}: renewing {}", holding.external);
        let renewed = match &mut holding.held {
            Held::Pcp(client) => client
                .renew(LIFETIME, &TRIES, note)
                .map_err(|err| err.to_string())
                .and_then(long_enough),
            Held::NatPmp(client) => client
                .renew(LIFETIME, &TRIES, note)
                .map_err(|err| err.to_string())
                .and_then(long_enough),
            Held::Upnp(held) => renew_upnp(held, note).map(|renewed| {
                if let Some(old) = renewed.retired {
                    log!(
                        self.log,
                        "upnp: udp {} may still be the room's on the router, deleted when the room closes",
                        old.external_port
                    );
                    self.stale.push(Held::Upnp(old));
                }
                (renewed.external, renewed.lease)
            }),
        };
        match renewed {
            Ok((external, lifetime)) => {
                if external == holding.external {
                    log!(self.log, "{word}: renewed {external} for {lifetime} s");
                } else {
                    log!(
                        self.log,
                        "{word}: renewed, but the router moved the mapping from {} to {external}, for {lifetime} s",
                        holding.external
                    );
                }
                holding.external = external;
                holding.lifetime = lifetime;
                holding.renew_at = renew_at(lifetime, least);
                (self.report)(holding.report());
                Some(holding)
            }
            Err(err) => {
                let old = holding.held;
                if self.closing() {
                    log!(
                        self.log,
                        "{word}: the renewal failed: {err}; the room is closing, so the router is not asked again"
                    );
                    self.stale.push(old);
                    return None;
                }
                log!(
                    self.log,
                    "{word}: the renewal failed: {err}; asking the router again from the start"
                );
                match self.ladder_and_report() {
                    Some(mut fresh) if old.same_as(&fresh.held) => {
                        fresh.held.take_over(&old);
                        Some(fresh)
                    }
                    fresh => {
                        self.stale.push(old);
                        fresh
                    }
                }
            }
        }
    }

    fn remap(&mut self, holding: Holding, gateway: Gateway) -> Option<Holding> {
        log!(
            self.log,
            "port mapping: this pc's outside address changed; deleting the mapping and asking the router again, since some routers keep listing a mapping that stopped working"
        );
        log!(
            self.log,
            "{}: deleting the mapping for {} before asking again",
            holding.held.protocol().word(),
            holding.external
        );
        self.delete(holding.held);
        if self.closing() {
            log!(
                self.log,
                "port mapping: room closed before the router was asked again"
            );
            return None;
        }
        self.retarget(gateway);
        self.ladder_and_report()
    }

    // The room held a mapping and lost it, most likely to a router that is
    // still coming back. None once the room closes.
    fn map_again(&mut self) -> Option<Holding> {
        let mut waits = self.target.again.into_iter();
        while !self.closing() {
            let wait = waits.next();
            match wait {
                Some(wait) => log!(
                    self.log,
                    "port mapping: asking the router again in {}",
                    secs(wait)
                ),
                None => log!(
                    self.log,
                    "port mapping: the router is not asked again until this pc's outside address changes"
                ),
            }
            match self.wait(wait.map(|wait| Instant::now() + wait)) {
                Wake::Closing => return None,
                Wake::Remap(gateway) => {
                    log!(
                        self.log,
                        "port mapping: this pc's outside address changed, asking the router again"
                    );
                    self.retarget(gateway);
                    waits = self.target.again.into_iter();
                }
                Wake::Renew => {}
            }
            if let Some(holding) = self.ladder_and_report() {
                return Some(holding);
            }
        }
        None
    }

    fn retarget(&mut self, gateway: Gateway) {
        if gateway != self.target.gateway {
            log!(
                self.log,
                "port mapping: the router to ask is {} now, this pc is {} to it",
                gateway.ip,
                gateway.local
            );
            self.target.moved_to(gateway);
        }
    }

    fn delete(&mut self, held: Held) {
        let log = self.log.clone();
        match held {
            Held::Pcp(mut client) => {
                let note = &mut |line: fmt::Arguments<'_>| log!(log, "pcp: {line}");
                match client.delete(&TRIES, note) {
                    Ok(()) => log!(log, "pcp: the router deleted the mapping"),
                    Err(err) => log!(
                        log,
                        "pcp: could not delete the mapping: {err}; the router drops it when its lifetime runs out"
                    ),
                }
            }
            Held::NatPmp(mut client) => {
                let note = &mut |line: fmt::Arguments<'_>| log!(log, "nat-pmp: {line}");
                match client.delete(&TRIES, note) {
                    Ok(()) => log!(log, "nat-pmp: the router deleted the mapping"),
                    Err(err) => log!(
                        log,
                        "nat-pmp: could not delete the mapping: {err}; the router drops it when its lifetime runs out"
                    ),
                }
            }
            Held::Upnp(held) if !held.delete_on_close => log!(
                log,
                "upnp: udp {} forwarded to this pc before the room asked, left in place",
                held.external_port
            ),
            Held::Upnp(held) => {
                if !still_the_rooms(&log, &held) {
                    return;
                }
                let port = held.external_port;
                log!(
                    log,
                    "upnp: asking {} to delete the mapping for udp {port}",
                    held.service.control
                );
                match upnp::delete_mapping(&held.service, port) {
                    Ok(()) => log!(log, "upnp: the router deleted the mapping for udp {port}"),
                    Err(err) if held.lease == 0 => log!(
                        log,
                        "upnp: could not delete the mapping for udp {port}: {err}; it has no lease and stays until it is removed in the router's settings"
                    ),
                    Err(err) => log!(
                        log,
                        "upnp: could not delete the mapping for udp {port}: {err}; the router drops it when its {} s lease runs out",
                        held.lease
                    ),
                }
            }
        }
    }
}

// DeletePortMapping names a port and nothing else, and a router like this
// one does not check who asks. A lease that ran out, or a router that
// restarted, can have given the port to another device by now, and a
// forward the user set up since is not the room's either. A router that
// keeps no names shows the room's own entry with none.
fn still_the_rooms(log: &Log, held: &UpnpHeld) -> bool {
    let port = held.external_port;
    match upnp::mapping_entry(&held.service, port) {
        Ok(entry) if entry.owner(held.internal) == Owner::Booth => true,
        Ok(entry) if entry.internal == held.internal && entry.description.is_empty() => true,
        Ok(entry) => {
            log!(
                log,
                "upnp: udp {port} forwards to {} (\"{}\") now, not the room's, left in place",
                entry.internal,
                entry.description
            );
            false
        }
        Err(UpnpError::Fault(_, Fault::NO_SUCH_ENTRY)) => {
            log!(
                log,
                "upnp: the router has no entry for udp {port} any more, nothing to delete"
            );
            false
        }
        Err(err) => {
            log!(
                log,
                "upnp: could not read the entry for udp {port}: {err}; deleting it all the same"
            );
            true
        }
    }
}

struct Renewed {
    external: SocketAddrV4,
    lease: u32,
    // The port held before, when the router gave another one. It may still
    // be the room's.
    retired: Option<UpnpHeld>,
}

// The address is asked for again, since a renewal is the one moment that
// shows it changed. Asked first, so a router that fails that has not just
// made a mapping nobody keeps track of.
fn renew_upnp(
    held: &mut UpnpHeld,
    note: &mut dyn FnMut(fmt::Arguments<'_>),
) -> Result<Renewed, String> {
    let ip = upnp::external_ip(&held.service).map_err(|err| err.to_string())?;
    let mapped = upnp::add_mapping(&held.service, held.internal, held.external_port, note)
        .map_err(|err| err.to_string())?;
    let retired = if mapped.external_port == held.external_port {
        held.delete_on_close |= mapped.delete_on_close;
        None
    } else {
        let old = held.clone();
        held.external_port = mapped.external_port;
        held.delete_on_close = mapped.delete_on_close;
        Some(old)
    };
    held.lease = mapped.lease;
    Ok(Renewed {
        external: SocketAddrV4::new(ip, mapped.external_port),
        lease: mapped.lease,
        retired,
    })
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
