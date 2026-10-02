use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use crate::adapters::{self, Adapter};
use crate::socket::unmap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AddrKind {
    Lan,
    Vpn,
    Ipv6Global,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalAddr {
    pub ip: IpAddr,
    pub kind: AddrKind,
    pub adapter: String,
    pub has_gateway: bool,
    // The adapter's first gateway of the same family as `ip`. An IPv6 one is
    // usually link-local, and the scope that would make it usable is not kept.
    pub gateway: Option<IpAddr>,
    // The adapter itself is Tailscale or WireGuard by name or description.
    // Kind is Vpn for any 100.64.0.0/10 address as well, so this is what tells
    // Tailscale apart from the PC's own adapter sitting behind carrier NAT.
    pub vpn_adapter: bool,
    // Windows marks the adapter as a real network card. Without a gateway
    // that is a plain switch or a cable with fixed addresses, which a friend
    // can reach; a virtual adapter without one is Hyper-V's or WSL's switch,
    // the Mobile Hotspot or a tunnel other than Tailscale and WireGuard. A
    // Hyper-V External switch or a network bridge is virtual as well though
    // it carries a real card, so on a network with no router the invite
    // leaves out an address on it that a friend could reach.
    pub hardware_adapter: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Path {
    Lan,
    Direct,
}

// The router to ask for a port, and this PC's address as that router sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gateway {
    pub ip: Ipv4Addr,
    pub local: Ipv4Addr,
}

// Only asks the routing table which adapter the default route uses; nothing
// is sent. TEST-NET-1 has no route of its own anywhere, so it takes that one.
const SOME_INTERNET_ADDRESS: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 9);

// This PC's addresses, best first: adapters with a gateway (the real LAN),
// then Tailscale and WireGuard, then network cards without a gateway, then
// the rest, virtual adapters without a gateway, which on a PC with Hyper-V or
// WSL are their vEthernet switches. The invite leaves out the LAN addresses
// of that last group; the log still lists them.
pub fn local_addresses() -> io::Result<Vec<LocalAddr>> {
    Ok(pick(adapters::list()?))
}

// Why there is no router to ask for a port. The names are the adapters'
// friendly names, which the user can set to anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoRouter {
    // The default route leaves through this Tailscale or WireGuard adapter,
    // so the room's traffic, STUN's included, never passes the home router.
    Tunnel(String),
    // It leaves through this adapter, which has no IPv4 router.
    NoGateway(String),
    Nowhere,
}

// A mapping to another of this PC's addresses would let packets in, but the
// replies leave from the address on the adapter the default route uses, and
// the router gives those a different outside port. It asks Windows for the
// route: call it when the addresses change, not per packet.
pub fn mapping_gateway(addrs: &[LocalAddr]) -> Result<Gateway, NoRouter> {
    let route = adapters::route_to(SOME_INTERNET_ADDRESS).ok();
    pick_gateway(addrs, route.as_ref().map(|(name, _)| name.as_str()))
}

// The router of the adapter the default route uses. Only when Windows would
// not say, or named an adapter that is not in the list, the first one in
// invite order, where adapters with a gateway come first.
fn pick_gateway(addrs: &[LocalAddr], route: Option<&str>) -> Result<Gateway, NoRouter> {
    let usable: Vec<(&str, Gateway)> = addrs
        .iter()
        .filter(|addr| !addr.vpn_adapter)
        .filter_map(|addr| match (addr.ip, addr.gateway) {
            (IpAddr::V4(local), Some(IpAddr::V4(ip))) => {
                Some((addr.adapter.as_str(), Gateway { ip, local }))
            }
            _ => None,
        })
        .collect();
    if let Some(route) = route {
        if let Some(&(_, gateway)) = usable.iter().find(|(adapter, _)| *adapter == route) {
            return Ok(gateway);
        }
        if let Some(addr) = addrs.iter().find(|addr| addr.adapter == route) {
            return Err(if addr.vpn_adapter {
                NoRouter::Tunnel(addr.adapter.clone())
            } else {
                NoRouter::NoGateway(addr.adapter.clone())
            });
        }
    }
    usable
        .first()
        .map(|&(_, gateway)| gateway)
        .ok_or(NoRouter::Nowhere)
}

pub fn is_lan_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_lan_ip(IpAddr::V4(v4)),
            None => v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local(),
        },
    }
}

// A peer at one of these is reached on the LAN or through a tunnel, never
// across this PC's router's outside, so what STUN sees of this PC means
// nothing to it. Loopback is left out: tests on one PC play the internet
// there.
pub fn is_inside(ip: IpAddr) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        IpAddr::V4(_) => ip,
    };
    if ip.is_loopback() {
        return false;
    }
    match ip {
        IpAddr::V4(v4) => is_lan_ip(ip) || is_shared_v4(v4),
        IpAddr::V6(_) => is_lan_ip(ip),
    }
}

// A private address alone does not make a LAN: WireGuard tunnels use the same
// 10.x and 192.168.x ranges, and Tailscale's IPv6 range is a ULA. A tunnel
// carries 1420 bytes at most (Tailscale 1280), so a peer behind one needs the
// internet packet size and pacing, which is what Direct means. Asks Windows
// for the route, so call it when the peer's address changes, not per packet.
pub fn path_of(peer: SocketAddr) -> Path {
    let peer = unmap(peer);
    let ip = peer.ip();
    if !is_lan_ip(ip) || is_tailscale_v6(ip) {
        return Path::Direct;
    }
    if ip.is_loopback() {
        return Path::Lan;
    }
    match adapters::route_to(peer) {
        Ok((name, description)) if is_vpn_name(&name, &description) => Path::Direct,
        // Without a route there is nothing to send through anyway, and the
        // address is the best guess left.
        _ => Path::Lan,
    }
}

// fd7a:115c:a1e0::/48, the fixed range Tailscale gives every device.
fn is_tailscale_v6(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V6(v6) => {
            let [a, b, c, ..] = v6.segments();
            (a, b, c) == (0xfd7a, 0x115c, 0xa1e0)
        }
        IpAddr::V4(_) => false,
    }
}

fn pick(adapters: Vec<Adapter>) -> Vec<LocalAddr> {
    let mut out: Vec<LocalAddr> = Vec::new();
    for adapter in adapters.iter().filter(|a| a.up) {
        let vpn = is_vpn_name(&adapter.name, &adapter.description);
        for addr in &adapter.addrs {
            // The temporary address changes daily, which to a strict router
            // looks like the host moving in the middle of a session.
            if !addr.preferred || (addr.ip.is_ipv6() && addr.random_suffix) {
                continue;
            }
            let Some(kind) = kind_of(addr.ip, vpn) else {
                continue;
            };
            if out.iter().any(|seen| seen.ip == addr.ip) {
                continue;
            }
            out.push(LocalAddr {
                ip: addr.ip,
                kind,
                adapter: adapter.name.clone(),
                has_gateway: adapter.has_gateway,
                // Some tunnels list 0.0.0.0, which is no router at all.
                gateway: adapter.gateways.iter().copied().find(|gateway| {
                    gateway.is_ipv4() == addr.ip.is_ipv4() && !gateway.is_unspecified()
                }),
                vpn_adapter: vpn,
                // An adapter Windows would not describe counts as virtual, so
                // without a gateway it stays out of invites, as all did before.
                hardware_adapter: adapter.hardware == Some(true),
            });
        }
    }
    out.sort_by_key(rank);
    out
}

fn rank(addr: &LocalAddr) -> (u8, u8) {
    let group = match (addr.has_gateway, addr.kind) {
        (true, _) => 0,
        (false, AddrKind::Vpn) => 1,
        (false, _) if addr.hardware_adapter => 2,
        (false, _) => 3,
    };
    let kind = match addr.kind {
        AddrKind::Lan => 0,
        AddrKind::Vpn => 1,
        AddrKind::Ipv6Global => 2,
    };
    (group, kind)
}

fn is_vpn_name(name: &str, description: &str) -> bool {
    [name, description].iter().any(|text| {
        let text = text.to_lowercase();
        text.contains("tailscale") || text.contains("wireguard")
    })
}

fn kind_of(ip: IpAddr, vpn_adapter: bool) -> Option<AddrKind> {
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() || v4.is_multicast() {
                None
            } else if vpn_adapter || is_shared_v4(v4) {
                Some(AddrKind::Vpn)
            } else if v4.is_private() {
                Some(AddrKind::Lan)
            } else {
                // A public IPv4 on the adapter itself: STUN reports the same
                // address with an easy mapping, so the invite gets it anyway.
                None
            }
        }
        IpAddr::V6(v6) if is_global_v6(v6) => Some(if vpn_adapter {
            AddrKind::Vpn
        } else {
            AddrKind::Ipv6Global
        }),
        IpAddr::V6(_) => None,
    }
}

// 100.64.0.0/10: carrier-grade NAT space, which Tailscale also hands out.
fn is_shared_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 100 && (b & 0xC0) == 64
}

// 2000::/3, minus Teredo (2001::/32) and 6to4 (2002::/16): both are tunnels
// through a relay somewhere on the internet, which is the opposite of direct.
fn is_global_v6(ip: Ipv6Addr) -> bool {
    let [first, second, ..] = ip.segments();
    (first & 0xE000) == 0x2000 && !(first == 0x2001 && second == 0) && first != 0x2002
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::AdapterAddr;

    fn adapter(name: &str, has_gateway: bool, ips: &[&str]) -> Adapter {
        Adapter {
            name: name.to_string(),
            description: String::new(),
            up: true,
            has_gateway,
            hardware: Some(false),
            gateways: Vec::new(),
            addrs: ips
                .iter()
                .map(|ip| AdapterAddr {
                    ip: ip.parse().unwrap(),
                    random_suffix: false,
                    preferred: true,
                })
                .collect(),
        }
    }

    fn card(mut adapter: Adapter) -> Adapter {
        adapter.hardware = Some(true);
        adapter
    }

    // Roughly my PC: Hyper-V and WSL adapters come first in Windows' own
    // order and must end up last. A second card on a plain switch has no
    // gateway either and goes before them.
    #[test]
    fn address_order() {
        let mut ethernet = card(adapter(
            "Ethernet",
            true,
            &["fe80::1", "2a02:8108::10", "192.168.1.20", "fd00::5"],
        ));
        let mut temporary = AdapterAddr {
            ip: "2a02:8108::abcd".parse().unwrap(),
            random_suffix: true,
            preferred: true,
        };
        ethernet.addrs.push(temporary);
        temporary.ip = "192.168.1.99".parse().unwrap();
        temporary.preferred = false;
        ethernet.addrs.push(temporary);

        let mut wifi = card(adapter("Wi-Fi", true, &["192.168.1.21"]));
        wifi.up = false;
        let mut tunnel = adapter("wg0", false, &["10.8.0.2"]);
        tunnel.description = "WireGuard Tunnel".to_string();
        let mut undescribed = adapter("ZeroTier One", false, &["10.147.17.5"]);
        undescribed.hardware = None;

        let picked = pick(vec![
            adapter(
                "vEthernet (WSL (Hyper-V firewall))",
                false,
                &["172.29.160.1"],
            ),
            card(adapter("Ethernet 2", false, &["10.0.0.5"])),
            adapter(
                "vEthernet (Default Switch)",
                false,
                &["172.20.112.1", "169.254.3.4"],
            ),
            wifi,
            ethernet,
            adapter(
                "Tailscale",
                false,
                &["100.101.102.103", "fd7a:115c:a1e0::1"],
            ),
            tunnel,
            undescribed,
            adapter("Loopback Pseudo-Interface 1", false, &["127.0.0.1", "::1"]),
        ]);

        let got: Vec<(String, AddrKind, bool)> = picked
            .iter()
            .map(|a| (a.ip.to_string(), a.kind, a.hardware_adapter))
            .collect();
        let want: Vec<(String, AddrKind, bool)> = [
            ("192.168.1.20", AddrKind::Lan, true),
            ("2a02:8108::10", AddrKind::Ipv6Global, true),
            ("100.101.102.103", AddrKind::Vpn, false),
            ("10.8.0.2", AddrKind::Vpn, false),
            ("10.0.0.5", AddrKind::Lan, true),
            ("172.29.160.1", AddrKind::Lan, false),
            ("172.20.112.1", AddrKind::Lan, false),
            ("10.147.17.5", AddrKind::Lan, false),
        ]
        .iter()
        .map(|(ip, kind, hardware)| (ip.to_string(), *kind, *hardware))
        .collect();
        assert_eq!(got, want);
        assert!(
            picked
                .first()
                .is_some_and(|a| a.adapter == "Ethernet" && a.has_gateway)
        );
    }

    #[test]
    fn tunnels_and_odd_ranges_are_not_candidates() {
        for ip in [
            "2001:0:4136:e378::1",
            "2002:c000:204::1",
            "8.8.8.8",
            "192.0.0.2",
            "ff02::1",
        ] {
            assert_eq!(kind_of(ip.parse().unwrap(), false), None, "{ip}");
        }
        assert_eq!(
            kind_of("100.64.0.1".parse().unwrap(), false),
            Some(AddrKind::Vpn)
        );
        assert_eq!(kind_of("100.128.0.1".parse().unwrap(), false), None);
        assert_eq!(
            kind_of("2001:db8::1".parse().unwrap(), true),
            Some(AddrKind::Vpn)
        );
    }

    // The second router check needs "this PC's own adapter has a 100.64/10
    // address", which the kind alone cannot say.
    #[test]
    fn carrier_nat_adapter_is_not_a_vpn() {
        let mut renamed = adapter("Ethernet 3", false, &["100.101.102.103"]);
        renamed.description = "Tailscale Tunnel".to_string();
        let picked = pick(vec![adapter("Ethernet", true, &["100.72.1.5"]), renamed]);
        let got: Vec<(String, AddrKind, bool)> = picked
            .iter()
            .map(|a| (a.ip.to_string(), a.kind, a.vpn_adapter))
            .collect();
        assert_eq!(
            got,
            vec![
                ("100.72.1.5".to_string(), AddrKind::Vpn, false),
                ("100.101.102.103".to_string(), AddrKind::Vpn, true),
            ]
        );
    }

    fn with_gateways(mut adapter: Adapter, gateways: &[&str]) -> Adapter {
        adapter.gateways = gateways.iter().map(|ip| ip.parse().unwrap()).collect();
        adapter.has_gateway = true;
        adapter
    }

    // My PC has Ethernet and Wi-Fi on the same router.
    #[test]
    fn gateways_and_the_router_to_ask() {
        let wifi = with_gateways(
            adapter("Wi-Fi", true, &["2a02:8108::37", "192.168.100.37"]),
            &["fe80::1", "192.168.100.1"],
        );
        let ethernet = with_gateways(
            adapter("Ethernet", true, &["192.168.100.38", "2a02:8108::38"]),
            &["192.168.100.1", "fe80::1"],
        );
        let mut tunnel = with_gateways(adapter("wg0", false, &["10.8.0.2"]), &["10.8.0.1"]);
        tunnel.description = "WireGuard Tunnel".to_string();
        let picked = pick(vec![
            tunnel,
            wifi,
            ethernet,
            adapter("vEthernet (Default Switch)", false, &["172.20.112.1"]),
        ]);

        let gateway_of = |ip: &str| {
            let ip: IpAddr = ip.parse().unwrap();
            picked.iter().find(|a| a.ip == ip).unwrap().gateway
        };
        let v4_router: IpAddr = "192.168.100.1".parse().unwrap();
        let v6_router: IpAddr = "fe80::1".parse().unwrap();
        assert_eq!(gateway_of("192.168.100.37"), Some(v4_router));
        assert_eq!(gateway_of("192.168.100.38"), Some(v4_router));
        assert_eq!(gateway_of("2a02:8108::37"), Some(v6_router));
        assert_eq!(gateway_of("172.20.112.1"), None);

        let router = Ipv4Addr::new(192, 168, 100, 1);
        let ethernet = Gateway {
            ip: router,
            local: Ipv4Addr::new(192, 168, 100, 38),
        };
        let wifi = Gateway {
            ip: router,
            local: Ipv4Addr::new(192, 168, 100, 37),
        };
        assert_eq!(pick_gateway(&picked, Some("Ethernet")), Ok(ethernet));
        assert_eq!(pick_gateway(&picked, Some("Wi-Fi")), Ok(wifi));
        // Windows would not say, or named an adapter the list left out.
        assert_eq!(pick_gateway(&picked, None), Ok(wifi));
        assert_eq!(pick_gateway(&picked, Some("Npcap Loopback")), Ok(wifi));

        let no_router: Vec<LocalAddr> = picked
            .iter()
            .filter(|a| a.adapter == "wg0" || a.ip.is_ipv6() || a.gateway.is_none())
            .cloned()
            .collect();
        assert_eq!(no_router.len(), 4);
        assert_eq!(pick_gateway(&no_router, None), Err(NoRouter::Nowhere));
    }

    // A full tunnel or an exit node carries the default route. The room's
    // traffic never passes the home router then, so opening a port on it
    // would only leave a useless forward to this PC.
    #[test]
    fn no_router_behind_a_tunnel() {
        let wifi = with_gateways(
            adapter("Wi-Fi", true, &["192.168.100.37"]),
            &["192.168.100.1"],
        );
        let mut tunnel = with_gateways(adapter("wg0", true, &["10.8.0.2"]), &["10.8.0.1"]);
        tunnel.description = "WireGuard Tunnel".to_string();
        let mut exit_node = adapter("Tailscale", false, &["100.101.102.103"]);
        exit_node.description = "Tailscale Tunnel".to_string();
        let lan_only = adapter("vEthernet (Default Switch)", false, &["172.20.112.1"]);
        let picked = pick(vec![wifi, tunnel, exit_node, lan_only]);

        assert_eq!(
            pick_gateway(&picked, Some("wg0")),
            Err(NoRouter::Tunnel("wg0".to_string()))
        );
        assert_eq!(
            pick_gateway(&picked, Some("Tailscale")),
            Err(NoRouter::Tunnel("Tailscale".to_string()))
        );
        assert_eq!(
            pick_gateway(&picked, Some("vEthernet (Default Switch)")),
            Err(NoRouter::NoGateway(
                "vEthernet (Default Switch)".to_string()
            ))
        );
    }

    #[test]
    fn carrier_nat_adapter_still_has_a_router_to_ask() {
        let picked = pick(vec![with_gateways(
            adapter("Ethernet", true, &["100.72.1.5"]),
            &["0.0.0.0", "100.64.0.1"],
        )]);
        assert_eq!(
            pick_gateway(&picked, Some("Ethernet")),
            Ok(Gateway {
                ip: Ipv4Addr::new(100, 64, 0, 1),
                local: Ipv4Addr::new(100, 72, 1, 5),
            })
        );
    }

    #[test]
    fn mapping_gateway_can_be_asked() {
        let found = local_addresses().expect("could not list local addresses");
        println!("router to ask for a port: {:?}", mapping_gateway(&found));
    }

    #[test]
    fn tunnel_adapters_by_name_or_description() {
        assert!(is_vpn_name("Tailscale", ""));
        assert!(is_vpn_name("Ethernet 3", "Tailscale Tunnel"));
        assert!(is_vpn_name("wg0", "WireGuard Tunnel"));
        assert!(is_vpn_name("WIREGUARD home", ""));
        assert!(!is_vpn_name(
            "Ethernet",
            "Intel(R) Ethernet Controller I225-V"
        ));
        assert!(!is_vpn_name(
            "vEthernet (Default Switch)",
            "Hyper-V Virtual Ethernet Adapter"
        ));
    }

    #[test]
    fn inside_addresses() {
        for ip in [
            "192.168.1.20",
            "10.8.0.2",
            "169.254.3.4",
            "100.101.102.103",
            "fd7a:115c:a1e0::1",
            "fd00::5",
            "fe80::1",
            "::ffff:192.168.1.20",
            "::ffff:100.64.0.1",
        ] {
            assert!(is_inside(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "203.0.113.7",
            "2a02:8108::10",
            "100.128.0.1",
            "127.0.0.1",
            "::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_inside(ip.parse().unwrap()), "{ip}");
        }
    }

    // path_of reads a failed lookup as "LAN", which would quietly hide a
    // lookup that never works.
    #[test]
    fn route_lookup_names_an_adapter() {
        for peer in ["127.0.0.1:41000", "[::1]:41000"] {
            let (name, _) = adapters::route_to(peer.parse().unwrap()).unwrap();
            assert!(!name.is_empty(), "{peer}");
        }
    }
}
