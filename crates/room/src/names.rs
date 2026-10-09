// The address name: a dynamic DNS name the host types into settings, which
// every invite carries. The host looks its own up when the room opens, to
// show whether it points here. A client looks it up when the invite's
// addresses have not answered in the fast round, and tries what it gives.
// A lookup can take a second or two, so it runs on a short-lived thread of
// its own (threads.rs), never on the receive or timer thread.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use invite::{Candidate, CandidateKind};
use net::dns::{self, DnsError, Nameservers, Resolved, Resolver};

use crate::config::Lookup;
use crate::log::{Log, list, log};
use crate::reply;
use crate::view::{NameAnswer, NameMatch, NameView};

// A name carries an address and no port, so each address is tried on the
// ports the invite's candidates use, this many at most.
const PORTS: usize = 3;
// A name can hold any number of addresses; the invite itself holds eight
// candidates, and the ladder sends to every one of them five times a second.
const MAX_FROM_NAME: usize = 8;

pub(crate) struct Request {
    pub name: String,
    pub servers: Option<Nameservers>,
    pub lookup: Lookup,
}

pub(crate) struct Outcome {
    pub servers: Option<Nameservers>,
    pub result: Result<Resolved, DnsError>,
}

// On the lookup thread. With a log, every step of it goes there.
pub(crate) fn look_up(request: Request, log: &Log) -> Outcome {
    let lookup = &request.lookup;
    let check = |addr: SocketAddr| lookup.check(addr);
    let resolver = Resolver {
        system: &*lookup.system,
        check: &check,
        port: lookup.port,
        timeout: dns::TIMEOUT,
    };
    let mut note = |line: fmt::Arguments<'_>| log!(log, "address name: {line}");
    let name = request.name.as_str();
    let servers = match request.servers {
        Some(servers) => {
            note(format_args!(
                "asking the nameservers of {} found before: {}",
                servers.zone,
                list(&servers.addrs)
            ));
            Some(servers)
        }
        None => match resolver.nameservers(name, &mut note) {
            Ok(servers) => Some(servers),
            Err(err) => {
                note(format_args!("{err}"));
                None
            }
        },
    };
    let result = resolver.resolve(name, servers.as_ref(), &mut note);
    if let Err(err) = &result {
        note(format_args!("{err}"));
    }
    Outcome { servers, result }
}

// The name and what came of looking it up.
pub(crate) struct AddressName {
    name: String,
    lookup: Lookup,
    // Found once and kept for the life of the room: they rarely move, and
    // finding them takes several lookups.
    servers: Option<Nameservers>,
    answer: NameAnswer,
    due: bool,
}

impl AddressName {
    pub(crate) fn new(name: String, lookup: Lookup) -> AddressName {
        AddressName {
            name,
            lookup,
            servers: None,
            answer: NameAnswer::NotAsked,
            due: false,
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn is_asked(&self) -> bool {
        self.due || self.answer != NameAnswer::NotAsked
    }

    pub(crate) fn has_answered(&self) -> bool {
        matches!(self.answer, NameAnswer::Found { .. })
    }

    // The lookup thread starts on the next timer pass. One already under way
    // is not asked twice.
    pub(crate) fn ask(&mut self) {
        if self.answer != NameAnswer::Looking {
            self.due = true;
        }
    }

    pub(crate) fn request(&mut self) -> Option<Request> {
        if !self.due {
            return None;
        }
        self.due = false;
        self.answer = NameAnswer::Looking;
        Some(Request {
            name: self.name.clone(),
            servers: self.servers.clone(),
            lookup: self.lookup.clone(),
        })
    }

    // Returns the addresses to use, every one of them passed the check.
    pub(crate) fn found(&mut self, outcome: Outcome) -> Vec<IpAddr> {
        if outcome.servers.is_some() {
            self.servers = outcome.servers;
        }
        self.answer = match outcome.result {
            Ok(resolved) => NameAnswer::Found {
                addrs: resolved.addrs.iter().map(|found| found.ip).collect(),
                refused: resolved
                    .refused
                    .iter()
                    .map(|refused| (refused.ip, refused.why))
                    .collect(),
            },
            Err(DnsError::NoSuchName(_)) => NameAnswer::NoSuchName,
            Err(DnsError::NoAddress(_)) => NameAnswer::NoAddress,
            // With the errors about a name that cannot be asked, which never
            // come: the host's name is checked when the room opens, and an
            // invite's when it is read.
            Err(_) => NameAnswer::Unanswered,
        };
        self.addrs().to_vec()
    }

    fn addrs(&self) -> &[IpAddr] {
        match &self.answer {
            NameAnswer::Found { addrs, .. } => addrs,
            _ => &[],
        }
    }

    pub(crate) fn v4(&self) -> impl Iterator<Item = Ipv4Addr> + '_ {
        self.addrs().iter().filter_map(|ip| match ip {
            IpAddr::V4(v4) => Some(*v4),
            IpAddr::V6(_) => None,
        })
    }

    // The host's check: the name's IPv4 address against this PC's outside
    // one. A name with several passes if any of them is this PC.
    pub(crate) fn matched(&self, outside: Option<Ipv4Addr>) -> Option<NameMatch> {
        let outside = outside?;
        let points_to = self
            .v4()
            .find(|ip| *ip == outside)
            .or_else(|| self.v4().next())?;
        Some(NameMatch { points_to, outside })
    }

    pub(crate) fn view(&self, outside: Option<NameMatch>) -> NameView {
        NameView {
            name: self.name.clone(),
            answer: self.answer.clone(),
            outside,
        }
    }
}

// The ports a client pairs each address from the name with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Ports {
    // Every public candidate's port, then the one the host is bound to,
    // which a port forward leads to.
    v4: Vec<u16>,
    // The invite's IPv6 candidate's port, or else the bound one.
    v6: Option<u16>,
}

impl Ports {
    // `also` is one more port worth trying, after the rest: where a known
    // host was reached last.
    pub(crate) fn of(candidates: &[Candidate], also: Option<u16>) -> Ports {
        let bound = reply::host_port(candidates);
        let public = candidates
            .iter()
            .filter(|c| c.kind == CandidateKind::Public)
            .map(|c| c.addr.port());
        let mut v4 = Vec::with_capacity(PORTS);
        for port in public.chain(bound).chain(also) {
            if v4.len() < PORTS && !v4.contains(&port) {
                v4.push(port);
            }
        }
        let v6 = candidates
            .iter()
            .find(|c| c.kind == CandidateKind::Ipv6)
            .map(|c| c.addr.port())
            .or(bound)
            .or(also);
        Ports { v4, v6 }
    }

    // Where to send for each address the name gave, leaving out what the
    // ladder already tries (`known`). Each address left out is written down
    // with the reason.
    pub(crate) fn targets(
        &self,
        ips: &[IpAddr],
        has_ipv6: bool,
        known: &[SocketAddr],
        log: &Log,
    ) -> Vec<SocketAddr> {
        let mut out: Vec<SocketAddr> = Vec::new();
        for &ip in ips {
            let ports: &[u16] = match ip {
                IpAddr::V4(_) => &self.v4,
                IpAddr::V6(_) if !has_ipv6 => {
                    log!(log, "address name: {ip} left out, this pc cannot send ipv6");
                    continue;
                }
                IpAddr::V6(_) => self.v6.as_slice(),
            };
            if ports.is_empty() {
                log!(
                    log,
                    "address name: {ip} left out, the invite shows no port to try it on"
                );
            }
            for &port in ports {
                let addr = SocketAddr::new(ip, port);
                if known.contains(&addr) {
                    log!(log, "address name: {addr} is in the invite already");
                } else if out.len() == MAX_FROM_NAME {
                    log!(
                        log,
                        "address name: {addr} left out, {MAX_FROM_NAME} addresses from the name are tried at most"
                    );
                } else if !out.contains(&addr) {
                    out.push(addr);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use invite::{Candidate, Invite, Mapping};
    use net::dns::{Found, Refused, Source};

    fn invite(candidates: &[(CandidateKind, &str)]) -> Invite {
        Invite {
            host_key: [9; 32],
            invite_id: [1; 8],
            secret: [3; 16],
            multi_use: false,
            expires_at: 1_800_000_000,
            candidates: candidates
                .iter()
                .map(|(kind, addr)| Candidate {
                    kind: *kind,
                    addr: addr.parse().unwrap(),
                })
                .collect(),
            mapping: Mapping::Easy,
            mapped: false,
            mapped_verified: false,
            second_router: false,
            hostname: Some(String::from("myroom.example.net")),
        }
    }

    fn ips(list: &[&str]) -> Vec<IpAddr> {
        list.iter().map(|ip| ip.parse().unwrap()).collect()
    }

    fn addrs(list: &[&str]) -> Vec<SocketAddr> {
        list.iter().map(|addr| addr.parse().unwrap()).collect()
    }

    // An invite from a home PC: the LAN address, the stable IPv6 one, the
    // mapped port, what STUN saw, and the outside address with the bound
    // port.
    fn home() -> Invite {
        invite(&[
            (CandidateKind::Lan, "192.168.1.20:41000"),
            (CandidateKind::Ipv6, "[2001:db8::20]:41000"),
            (CandidateKind::Public, "203.0.113.9:41005"),
            (CandidateKind::Public, "203.0.113.9:52000"),
            (CandidateKind::Public, "203.0.113.9:41000"),
        ])
    }

    #[test]
    fn new_address_on_every_port() {
        let invite = home();
        let ports = Ports::of(&invite.candidates, None);
        let known: Vec<SocketAddr> = invite.candidates.iter().map(|c| c.addr).collect();
        let targets = ports.targets(&ips(&["198.51.100.4"]), true, &known, &Log::off());
        assert_eq!(
            targets,
            addrs(&[
                "198.51.100.4:41005",
                "198.51.100.4:52000",
                "198.51.100.4:41000",
            ])
        );
    }

    #[test]
    fn three_ports_at_most() {
        let invite = invite(&[
            (CandidateKind::Public, "203.0.113.9:1001"),
            (CandidateKind::Public, "203.0.113.9:1002"),
            (CandidateKind::Public, "203.0.113.9:1002"),
            (CandidateKind::Public, "203.0.113.9:1003"),
            (CandidateKind::Public, "203.0.113.9:1004"),
        ]);
        let ports = Ports::of(&invite.candidates, None);
        assert_eq!(ports.v4, [1001, 1002, 1003]);
        // Without a local candidate the bound port is the last public one's.
        assert_eq!(ports.v6, Some(1004));

        let only_local = Ports::of(
            &self::invite(&[(CandidateKind::Lan, "0.0.0.9:41000")]).candidates,
            None,
        );
        assert_eq!(only_local.v4, [41000]);
        assert_eq!(only_local.v6, Some(41000));
    }

    #[test]
    fn invite_addresses_not_tried_twice() {
        let invite = home();
        let ports = Ports::of(&invite.candidates, None);
        let known: Vec<SocketAddr> = invite.candidates.iter().map(|c| c.addr).collect();
        // The address did not change; only a port the invite lacks is new.
        let (log, captured) = Log::capture(16);
        let targets = ports.targets(&ips(&["203.0.113.9"]), true, &known, &log);
        assert!(targets.is_empty(), "{targets:?}");
        assert_eq!(
            captured.lines(),
            [
                "address name: 203.0.113.9:41005 is in the invite already",
                "address name: 203.0.113.9:52000 is in the invite already",
                "address name: 203.0.113.9:41000 is in the invite already",
            ]
        );
    }

    #[test]
    fn ipv6_port_and_reach() {
        let mut invite = home();
        invite.candidates[1].addr = "[2001:db8::20]:41010".parse().unwrap();
        let ports = Ports::of(&invite.candidates, None);
        let found = ips(&["2001:db8::99", "198.51.100.4"]);
        let targets = ports.targets(&found, true, &[], &Log::off());
        assert_eq!(targets[0], "[2001:db8::99]:41010".parse().unwrap());
        assert_eq!(targets.len(), 4);

        let (log, captured) = Log::capture(16);
        let targets = ports.targets(&found, false, &[], &log);
        assert!(targets.iter().all(SocketAddr::is_ipv4), "{targets:?}");
        assert_eq!(
            captured.lines(),
            ["address name: 2001:db8::99 left out, this pc cannot send ipv6"]
        );

        // No IPv6 candidate: the bound port, from the LAN address.
        invite.candidates.remove(1);
        let targets =
            Ports::of(&invite.candidates, None).targets(&found[..1], true, &[], &Log::off());
        assert_eq!(targets, addrs(&["[2001:db8::99]:41000"]));
    }

    #[test]
    fn eight_addresses_at_most() {
        let invite = home();
        let many: Vec<IpAddr> = (1..=5)
            .map(|n| IpAddr::V4(Ipv4Addr::new(198, 51, 100, n)))
            .collect();
        let targets = Ports::of(&invite.candidates, None).targets(&many, true, &[], &Log::off());
        assert_eq!(targets.len(), MAX_FROM_NAME);
        assert_eq!(targets[0], "198.51.100.1:41005".parse().unwrap());
    }

    fn found(v4: &[&str]) -> Outcome {
        Outcome {
            servers: None,
            result: Ok(Resolved {
                addrs: ips(v4)
                    .into_iter()
                    .map(|ip| Found {
                        ip,
                        source: Source::Authoritative,
                    })
                    .collect(),
                refused: vec![Refused {
                    ip: "127.0.0.1".parse().unwrap(),
                    why: "it is a loopback address",
                }],
            }),
        }
    }

    #[test]
    fn host_check_prefers_this_pc() {
        let outside = Ipv4Addr::new(198, 51, 100, 20);
        let mut name = AddressName::new(String::from("myroom.example.net"), Lookup::default());
        assert_eq!(name.matched(Some(outside)), None);
        name.found(found(&["2001:db8::1", "203.0.113.5", "198.51.100.20"]));
        let here = name.matched(Some(outside)).expect("both are known");
        assert_eq!(here.points_to, outside);
        assert!(here.is_this_pc());
        assert_eq!(name.matched(None), None);

        name.found(found(&["203.0.113.5", "203.0.113.6"]));
        let elsewhere = name.matched(Some(outside)).expect("both are known");
        assert_eq!(elsewhere.points_to, Ipv4Addr::new(203, 0, 113, 5));
        assert!(!elsewhere.is_this_pc());

        name.found(found(&["2001:db8::1"]));
        assert_eq!(name.matched(Some(outside)), None);
    }

    #[test]
    fn lookup_once_per_ask() {
        let mut name = AddressName::new(String::from("myroom.example.net"), Lookup::default());
        assert!(!name.is_asked());
        assert!(name.request().is_none());
        name.ask();
        assert!(name.is_asked());
        let first = name.request().expect("asked");
        assert!(first.servers.is_none());
        assert_eq!(name.view(None).answer, NameAnswer::Looking);
        name.ask();
        assert!(name.request().is_none(), "asked again while looking");

        let servers = Nameservers {
            zone: String::from("example.net"),
            names: vec![String::from("ns1.example.net")],
            addrs: vec!["198.51.100.53:53".parse().unwrap()],
        };
        let mut outcome = found(&["203.0.113.5"]);
        outcome.servers = Some(servers.clone());
        assert_eq!(name.found(outcome), ips(&["203.0.113.5"]));
        assert_eq!(
            name.view(None).answer,
            NameAnswer::Found {
                addrs: ips(&["203.0.113.5"]),
                refused: vec![("127.0.0.1".parse().unwrap(), "it is a loopback address")],
            }
        );
        name.ask();
        assert_eq!(name.request().expect("asked again").servers, Some(servers));

        let gone = Outcome {
            servers: None,
            result: Err(DnsError::NoSuchName(String::from("myroom.example.net"))),
        };
        assert!(name.found(gone).is_empty());
        assert_eq!(name.view(None).answer, NameAnswer::NoSuchName);
    }
}
