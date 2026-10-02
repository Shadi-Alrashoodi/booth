// What the host knows about the routers between it and the internet, from
// STUN (stun.rs) and the port mapping (mapper.rs): the sentence the panel
// shows, the mapped address and flags for the invite, and the check for a
// second router in front of this one, which port mapping cannot cross.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use crate::log::{Log, log};
use crate::mapper::{Protocol, Report};
use crate::view::RouterState;

pub(crate) struct PortMap {
    // A mapper thread is asking the router. Not with a fixed address list,
    // or when no adapter has a router to ask.
    asking: bool,
    answered: bool,
    mapped: Option<(Protocol, SocketAddrV4)>,
    // The outside address the router reported, with a mapping or without.
    wan: Option<(Protocol, Ipv4Addr)>,
    // What STUN saw when the router last answered, or the first STUN answer
    // after that. The router's address is judged against this one only:
    // STUN asks every 20 s and the router is asked every hour, so a later
    // STUN answer that differs is an address change the router has not been
    // asked about, not a second router.
    stun: Option<Ipv4Addr>,
    // The STUN address the router was last asked again about, so each
    // change asks once.
    asked_about: Option<Ipv4Addr>,
    verified: Option<SocketAddrV4>,
    // The adapter toward the internet has a carrier-grade NAT address.
    carrier_nat: Option<Ipv4Addr>,
    // The last verdict written down, so each change is written once.
    said: Option<String>,
    log: Log,
}

// What goes into an invite.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Offer {
    // None behind a second router: the mapping is on the inner router, and
    // nobody outside can reach it.
    pub mapped: Option<SocketAddrV4>,
    pub verified: bool,
    pub second_router: bool,
}

impl PortMap {
    pub(crate) fn new(asking: bool, carrier_nat: Option<Ipv4Addr>, log: Log) -> PortMap {
        PortMap {
            asking,
            answered: false,
            mapped: None,
            wan: None,
            stun: None,
            asked_about: None,
            verified: None,
            carrier_nat,
            said: None,
            log,
        }
    }

    // The first try of the ladder is over, or there is none.
    pub(crate) fn is_settled(&self) -> bool {
        !self.asking || self.answered
    }

    // `stun` is what STUN saw last, which this answer is judged against.
    pub(crate) fn report(&mut self, report: Report, stun: Option<Ipv4Addr>) {
        self.answered = true;
        self.stun = stun;
        // A failed renewal that reached no router keeps the address the
        // router gave before, the best there is.
        let wan = match report {
            Report::Mapped {
                protocol, external, ..
            } => {
                let usable = self.usable(protocol, *external.ip());
                self.mapped = usable.then_some((protocol, external));
                usable.then_some((protocol, *external.ip()))
            }
            Report::Unmapped { wan } => {
                self.mapped = None;
                wan.filter(|&(protocol, ip)| self.usable(protocol, ip))
            }
        };
        if wan.is_some() {
            self.wan = wan;
        }
    }

    // Not an address on the internet, and not a second router's either: the
    // router has no outside address yet, or it answered nonsense. The invite
    // would leave such a candidate out, so the panel must not offer it.
    fn usable(&self, protocol: Protocol, ip: Ipv4Addr) -> bool {
        let why = match ip.octets() {
            [0, ..] | [240..=255, ..] => "a reserved address",
            [127, ..] => "a loopback address",
            [169, 254, ..] => "a link-local address, so the router has no outside address yet",
            [224..=239, ..] => "a multicast address",
            _ => return true,
        };
        log!(
            self.log,
            "port mapping: {} says the router's outside address is {ip}, {why}; not used",
            protocol.name()
        );
        false
    }

    // Every STUN answer comes here. Returns true when the router should be
    // asked again: STUN sees another address than it did when the router
    // last answered, and the router said neither.
    pub(crate) fn stun_seen(&mut self, seen: Option<Ipv4Addr>) -> bool {
        let Some(seen) = seen else {
            return false;
        };
        let wan = self.wan.map(|(_, wan)| wan);
        match self.stun {
            Some(paired) if paired == seen => false,
            Some(paired) if wan.is_some() && wan != Some(seen) => {
                if self.asked_about == Some(seen) {
                    return false;
                }
                self.asked_about = Some(seen);
                let next = if self.mapped.is_some() {
                    "asking the router again before judging"
                } else {
                    "the verdict stays as it was until the router answers again"
                };
                log!(
                    self.log,
                    "port mapping: stun sees {seen} now and saw {paired} when the router last answered; {next}"
                );
                self.mapped.is_some()
            }
            // The first STUN answer since the router's, or one that now
            // agrees with it.
            _ => {
                self.stun = Some(seen);
                false
            }
        }
    }

    // A friend's client says it reached the host at `addr`. Returns the
    // mapping that just proved itself. It is that friend's claim and nothing
    // more: a lying client makes the panel say "verified", and every later
    // invite carry mapped_verified, so whoever reads that flag must take it
    // as a hint and never skip a way in because of it.
    pub(crate) fn reached(&mut self, addr: SocketAddr) -> Option<(Protocol, SocketAddrV4)> {
        let SocketAddr::V4(addr) = addr else {
            return None;
        };
        match self.mapped {
            Some((protocol, mapped)) if mapped == addr && self.verified != Some(addr) => {
                self.verified = Some(addr);
                Some((protocol, addr))
            }
            _ => None,
        }
    }

    pub(crate) fn protocol(&self) -> Option<Protocol> {
        self.mapped.map(|(protocol, _)| protocol)
    }

    pub(crate) fn mapped(&self) -> Option<SocketAddrV4> {
        self.mapped.map(|(_, addr)| addr)
    }

    // The outside address the router reported, with a mapping or without,
    // when it can be the one the internet sees. A second router's private
    // or carrier-grade NAT address is not, and neither is anything a router
    // behind carrier-grade NAT reports.
    pub(crate) fn outside(&self) -> Option<Ipv4Addr> {
        let (_, wan) = self.wan?;
        (self.carrier_nat.is_none() && not_outside(wan).is_none()).then_some(wan)
    }

    fn is_verified(&self) -> bool {
        self.mapped
            .is_some_and(|(_, addr)| self.verified == Some(addr))
    }

    // STUN's own answer is always a public address, so it cannot show a
    // second router by itself.
    pub(crate) fn state(&self, stun_state: RouterState) -> RouterState {
        if stun_state == RouterState::Testing || !self.is_settled() {
            RouterState::Testing
        } else if self.carrier_nat.is_some() {
            RouterState::CarrierNat
        } else if self.second_router().is_some() {
            RouterState::SecondRouter
        } else if self.is_verified() {
            RouterState::MappedVerified
        } else if self.mapped.is_some() {
            RouterState::Mapped
        } else {
            stun_state
        }
    }

    pub(crate) fn offer(&self) -> Offer {
        let second_router = self.second_router().is_some();
        let mapped = self.mapped().filter(|_| !second_router);
        Offer {
            mapped,
            verified: mapped.is_some() && self.is_verified(),
            second_router,
        }
    }

    // Why there is a router between this one and the internet, or None when
    // nothing shows one. Without an answer from a mapping protocol a second
    // router cannot be told apart from a strict home router.
    fn second_router(&self) -> Option<String> {
        if let Some(ip) = self.carrier_nat {
            return Some(format!(
                "this pc's own adapter has {ip}, a carrier-grade nat address"
            ));
        }
        let (protocol, wan) = self.wan?;
        let says = format!(
            "{} says the router's outside address is {wan}",
            protocol.name()
        );
        if let Some(what) = not_outside(wan) {
            return Some(format!("{says}, {what}"));
        }
        match self.stun {
            Some(seen) if seen != wan => Some(format!("{says}, stun saw {seen}")),
            _ => None,
        }
    }

    // Written when it first becomes known and again whenever it changes.
    pub(crate) fn note_verdict(&mut self) {
        if !self.log.is_on() {
            return;
        }
        let verdict = match (self.second_router(), self.wan) {
            (Some(why), _) => format!("second router: yes, {why}"),
            (None, _) if !self.is_settled() => return,
            (None, Some((protocol, wan))) => {
                let stun = self.stun.map_or_else(
                    || String::from("stun has not answered"),
                    |seen| format!("stun saw {seen}"),
                );
                format!(
                    "second router: none seen, {} says the router's outside address is {wan}, {stun}",
                    protocol.name()
                )
            }
            (None, None) if self.asking => {
                String::from("second router: not known, no mapping protocol answered")
            }
            (None, None) => String::from("second router: not known, no router was asked"),
        };
        if self.said.as_ref() != Some(&verdict) {
            log!(self.log, "{verdict}");
            self.said = Some(verdict);
        }
    }
}

// This PC's address on the adapter toward the internet, when it is in
// 100.64.0.0/10. Any other adapter in that range is some overlay network,
// Tailscale, WARP or NetBird among them, and says nothing about the way out.
pub(crate) fn carrier_nat(toward_internet: Ipv4Addr) -> Option<Ipv4Addr> {
    is_carrier_nat(toward_internet).then_some(toward_internet)
}

fn is_carrier_nat(ip: Ipv4Addr) -> bool {
    matches!(ip.octets(), [100, 64..=127, ..])
}

// An outside address no router facing the internet would have.
fn not_outside(ip: Ipv4Addr) -> Option<&'static str> {
    if ip.is_private() {
        Some("a private address")
    } else if is_carrier_nat(ip) {
        Some("a carrier-grade nat address")
    } else if matches!(ip.octets(), [192, 0, 0, 0..=7]) {
        Some("a ds-lite address")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STUN: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 9);
    const OTHER: Ipv4Addr = Ipv4Addr::new(198, 51, 100, 4);

    fn mapped_at(ip: Ipv4Addr, port: u16) -> Report {
        Report::Mapped {
            protocol: Protocol::Pcp,
            external: SocketAddrV4::new(ip, port),
            lifetime: 7200,
        }
    }

    // The router answered while STUN had seen `stun`.
    fn asked_with(report: Report, stun: Option<Ipv4Addr>) -> PortMap {
        let mut map = PortMap::new(true, None, Log::off());
        map.report(report, stun);
        map
    }

    fn asked(report: Report) -> PortMap {
        asked_with(report, Some(STUN))
    }

    #[test]
    fn private_outside_is_second_router() {
        for (wan, why) in [
            ("192.168.1.1", "a private address"),
            ("10.20.0.5", "a private address"),
            ("172.31.255.1", "a private address"),
            ("100.64.0.1", "a carrier-grade nat address"),
            ("100.127.255.254", "a carrier-grade nat address"),
            ("192.0.0.2", "a ds-lite address"),
        ] {
            let wan: Ipv4Addr = wan.parse().unwrap();
            let map = asked(mapped_at(wan, 41000));
            let found = map.second_router().expect("a second router");
            assert!(found.ends_with(why), "{wan}: {found}");
            assert_eq!(map.state(RouterState::Easy), RouterState::SecondRouter);
            let offer = map.offer();
            assert!(offer.second_router);
            assert_eq!(offer.mapped, None, "{wan}");
        }
        // Next to the ranges, and so a real outside address.
        for wan in ["100.128.0.1", "192.0.0.8", "172.32.0.1"] {
            let wan: Ipv4Addr = wan.parse().unwrap();
            let map = asked_with(mapped_at(wan, 41000), Some(wan));
            assert_eq!(map.second_router(), None);
        }
    }

    // A router whose outside has no lease, or one that answers nonsense, is
    // neither a mapping to offer nor a second router.
    #[test]
    fn impossible_outside_is_no_mapping() {
        let (log, captured) = Log::capture(64);
        for wan in [
            "169.254.3.4",
            "0.0.0.7",
            "127.0.0.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
        ] {
            let wan: Ipv4Addr = wan.parse().unwrap();
            let mut map = PortMap::new(true, None, log.clone());
            map.report(mapped_at(wan, 41000), Some(STUN));
            assert_eq!(map.mapped(), None, "{wan}");
            assert_eq!(map.protocol(), None);
            assert_eq!(map.second_router(), None, "{wan}");
            assert_eq!(map.state(RouterState::Easy), RouterState::Easy);
            let offer = map.offer();
            assert_eq!((offer.mapped, offer.second_router), (None, false));

            map.report(
                Report::Unmapped {
                    wan: Some((Protocol::NatPmp, wan)),
                },
                Some(STUN),
            );
            assert_eq!(map.second_router(), None, "{wan}");
        }
        let lines = captured.lines();
        assert!(
            lines.contains(&String::from(
                "port mapping: PCP says the router's outside address is 169.254.3.4, a link-local address, so the router has no outside address yet; not used"
            )),
            "{lines:#?}"
        );
    }

    #[test]
    fn router_disagreeing_with_stun() {
        let map = asked(mapped_at(OTHER, 41000));
        let why = map.second_router().expect("a second router");
        assert_eq!(
            why,
            "PCP says the router's outside address is 198.51.100.4, stun saw 203.0.113.9"
        );
        // Without a STUN answer there is nothing to disagree with.
        let early = asked_with(mapped_at(OTHER, 41000), None);
        assert_eq!(early.second_router(), None);
        assert_eq!(early.state(RouterState::Unknown), RouterState::Mapped);
    }

    // STUN asks every 20 s and the router every hour. When the provider
    // changes the address in between, STUN sees it first, and that is no
    // second router.
    #[test]
    fn later_stun_asks_router_again() {
        let (log, captured) = Log::capture(64);
        let mut map = PortMap::new(true, None, log);
        map.report(mapped_at(STUN, 41000), Some(STUN));
        assert!(!map.stun_seen(Some(STUN)));

        assert!(map.stun_seen(Some(OTHER)), "ask the router again");
        assert_eq!(map.state(RouterState::Easy), RouterState::Mapped);
        assert!(!map.offer().second_router);
        // Once per address STUN moves to.
        assert!(!map.stun_seen(Some(OTHER)));
        assert!(!map.stun_seen(Some(STUN)));
        assert!(!map.stun_seen(Some(OTHER)));
        assert_eq!(
            captured.lines(),
            [
                "port mapping: stun sees 198.51.100.4 now and saw 203.0.113.9 when the router last answered; asking the router again before judging"
            ]
        );

        // The renewal comes back with the new address.
        map.report(mapped_at(OTHER, 41000), Some(OTHER));
        assert_eq!(map.second_router(), None);
        assert_eq!(map.mapped(), Some(SocketAddrV4::new(OTHER, 41000)));
    }

    #[test]
    fn stun_catching_up_clears_verdict() {
        // The router answered with the new address before STUN saw it.
        let mut map = asked_with(mapped_at(OTHER, 41000), Some(STUN));
        assert!(map.second_router().is_some());
        assert!(!map.stun_seen(Some(OTHER)));
        assert_eq!(map.second_router(), None);

        // The first STUN answer after a router that answered first.
        let mut early = asked_with(mapped_at(OTHER, 41000), None);
        assert!(!early.stun_seen(Some(STUN)));
        assert!(early.second_router().is_some());
    }

    // The host holds its address name against this when STUN has not
    // answered, so it must be an address the name could rightly point to.
    #[test]
    fn outside_must_be_on_internet() {
        assert_eq!(
            asked_with(mapped_at(OTHER, 41000), None).outside(),
            Some(OTHER)
        );
        let refused = asked_with(
            Report::Unmapped {
                wan: Some((Protocol::NatPmp, OTHER)),
            },
            None,
        );
        assert_eq!(refused.outside(), Some(OTHER));
        for wan in ["192.168.0.10", "100.64.0.1", "192.0.0.2"] {
            let wan: Ipv4Addr = wan.parse().unwrap();
            let map = asked_with(mapped_at(wan, 41000), None);
            assert_eq!(map.outside(), None, "{wan}");
        }
        let mut behind_cgnat =
            PortMap::new(true, carrier_nat(Ipv4Addr::new(100, 72, 1, 5)), Log::off());
        behind_cgnat.report(mapped_at(OTHER, 41000), None);
        assert_eq!(behind_cgnat.outside(), None);
        assert_eq!(PortMap::new(true, None, Log::off()).outside(), None);
    }

    #[test]
    fn refused_mapping_tells_outside() {
        let map = asked(Report::Unmapped {
            wan: Some((Protocol::Upnp, Ipv4Addr::new(100, 70, 1, 2))),
        });
        assert_eq!(map.state(RouterState::Easy), RouterState::SecondRouter);
        let mut silent = asked(Report::Unmapped { wan: None });
        assert_eq!(silent.second_router(), None);
        assert_eq!(silent.state(RouterState::Easy), RouterState::Easy);
        // Nothing to ask again either.
        assert!(!silent.stun_seen(Some(OTHER)));

        // With nothing mapped the mapper has nothing to renew, and the
        // verdict stays with what STUN saw when the router answered.
        let mut refused = asked(Report::Unmapped {
            wan: Some((Protocol::NatPmp, STUN)),
        });
        assert!(!refused.stun_seen(Some(OTHER)));
        assert_eq!(refused.second_router(), None);
    }

    #[test]
    fn carrier_nat_from_internet_adapter() {
        assert_eq!(
            carrier_nat(Ipv4Addr::new(100, 72, 1, 5)),
            Some(Ipv4Addr::new(100, 72, 1, 5))
        );
        assert_eq!(carrier_nat(Ipv4Addr::new(192, 168, 1, 20)), None);
        assert_eq!(carrier_nat(Ipv4Addr::new(100, 128, 0, 1)), None);

        // Known before any router answers, and without one being asked. The
        // panel tells it apart from a box in front of a home router.
        let map = PortMap::new(false, carrier_nat(Ipv4Addr::new(100, 72, 1, 5)), Log::off());
        assert_eq!(map.state(RouterState::Unknown), RouterState::CarrierNat);
        assert!(map.offer().second_router);
        let boxed = asked(mapped_at(Ipv4Addr::new(192, 168, 0, 1), 41000));
        assert_eq!(boxed.state(RouterState::Easy), RouterState::SecondRouter);
    }

    #[test]
    fn verified_by_mapped_address() {
        let mapped = SocketAddrV4::new(STUN, 41000);
        let mut map = asked(mapped_at(STUN, 41000));
        assert_eq!(map.state(RouterState::Easy), RouterState::Mapped);
        assert_eq!(map.reached("192.168.1.20:41000".parse().unwrap()), None);
        assert_eq!(map.reached("203.0.113.9:52000".parse().unwrap()), None);
        assert_eq!(map.reached("[2001:db8::1]:41000".parse().unwrap()), None);
        assert_eq!(map.state(RouterState::Easy), RouterState::Mapped);

        assert_eq!(
            map.reached(SocketAddr::V4(mapped)),
            Some((Protocol::Pcp, mapped))
        );
        // Said once, not for every friend who comes in the same way.
        assert_eq!(map.reached(SocketAddr::V4(mapped)), None);
        assert_eq!(map.state(RouterState::Easy), RouterState::MappedVerified);
        let offer = map.offer();
        assert_eq!(offer.mapped, Some(mapped));
        assert!(offer.verified);

        // A renewal that lands on another port is not the one a friend used.
        map.report(mapped_at(STUN, 41001), Some(STUN));
        assert_eq!(map.state(RouterState::Easy), RouterState::Mapped);
        assert!(!map.offer().verified);
        map.report(Report::Unmapped { wan: None }, Some(STUN));
        assert_eq!(map.state(RouterState::Easy), RouterState::Easy);
        assert_eq!(map.offer().mapped, None);
    }

    #[test]
    fn testing_until_both_settle() {
        let waiting = PortMap::new(true, None, Log::off());
        assert!(!waiting.is_settled());
        assert_eq!(waiting.state(RouterState::Easy), RouterState::Testing);
        let map = asked(mapped_at(STUN, 41000));
        assert_eq!(map.state(RouterState::Testing), RouterState::Testing);
        let off = PortMap::new(false, None, Log::off());
        assert!(off.is_settled());
        assert_eq!(off.state(RouterState::Hard), RouterState::Hard);
    }

    #[test]
    fn the_verdict_is_written_when_it_changes() {
        let (log, captured) = Log::capture(64);
        let mut map = PortMap::new(true, None, log);
        map.note_verdict();
        assert!(captured.lines().is_empty(), "nothing before the ladder");
        map.report(mapped_at(STUN, 41000), Some(STUN));
        map.note_verdict();
        map.note_verdict();
        map.report(mapped_at(OTHER, 41000), Some(STUN));
        map.note_verdict();
        assert_eq!(
            captured.lines(),
            [
                "second router: none seen, PCP says the router's outside address is 203.0.113.9, stun saw 203.0.113.9",
                "second router: yes, PCP says the router's outside address is 198.51.100.4, stun saw 203.0.113.9",
            ]
        );
    }
}
