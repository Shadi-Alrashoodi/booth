use std::net::{IpAddr, SocketAddr};

use net::addrs::{AddrKind, Path, is_lan_ip, local_addresses, path_of};

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

#[test]
fn lan_ranges() {
    for lan in [
        "10.0.0.1",
        "10.255.255.255",
        "172.16.0.1",
        "172.31.255.254",
        "192.168.1.20",
        "169.254.10.10",
        "127.0.0.1",
        "::1",
        "fd7a:115c:a1e0::1",
        "fc00::1",
        "fe80::1",
        "::ffff:192.168.1.20",
        "::ffff:10.1.2.3",
    ] {
        assert!(is_lan_ip(ip(lan)), "{lan} should be lan");
    }
    for not_lan in [
        "100.64.0.1",
        "100.101.102.103",
        "100.127.255.255",
        "172.32.0.1",
        "172.15.255.255",
        "192.169.0.1",
        "8.8.8.8",
        "203.0.113.7",
        "2001:db8::1",
        "2a02:8108::10",
        "fec0::1",
        "::ffff:100.64.0.1",
        "::ffff:8.8.8.8",
        "0.0.0.0",
        "::",
    ] {
        assert!(!is_lan_ip(ip(not_lan)), "{not_lan} should not be lan");
    }
}

#[test]
fn path_word_follows_the_peer_address() {
    let peer = |s: &str| s.parse::<SocketAddr>().unwrap();
    // Tailscale is not the LAN, even though it feels like one, and its IPv6
    // range is a ULA, which is why the address rule alone got it wrong.
    for direct in [
        "203.0.113.7:41000",
        "100.101.102.103:41000",
        "[::ffff:100.64.0.1]:41000",
        "[2a02:8108::10]:41000",
        "[fd7a:115c:a1e0::1]:41000",
        "[fd7a:115c:a1e0:ab12::3]:41000",
    ] {
        assert_eq!(path_of(peer(direct)), Path::Direct, "{direct}");
    }
    for lan in ["127.0.0.1:41000", "[::1]:41000", "[::ffff:127.0.0.1]:41000"] {
        assert_eq!(path_of(peer(lan)), Path::Lan, "{lan}");
    }
}

// Private addresses are decided by the route, so this depends on the PC; but
// whatever its routes, its own LAN addresses are not behind a tunnel.
#[test]
fn own_lan_addresses_are_lan() {
    let found = local_addresses().expect("could not list local addresses");
    for a in found.iter().filter(|a| a.kind == AddrKind::Lan) {
        let peer = SocketAddr::new(a.ip, 41000);
        assert_eq!(path_of(peer), Path::Lan, "{peer} on {}", a.adapter);
    }
}

#[test]
fn local_addresses_can_be_listed() {
    let found = local_addresses().expect("could not list local addresses");
    println!("{} candidate addresses on this pc:", found.len());
    for a in &found {
        println!(
            "  {:<40} {:<11} gateway={:<5} vpn adapter={:<5} hardware={:<5} path={:<6} {}",
            a.ip.to_string(),
            format!("{:?}", a.kind),
            a.has_gateway,
            a.vpn_adapter,
            a.hardware_adapter,
            format!("{:?}", path_of(SocketAddr::new(a.ip, 41000))),
            a.adapter
        );
    }
}
