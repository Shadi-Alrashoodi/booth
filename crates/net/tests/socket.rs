use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use net::addrs::{AddrKind, local_addresses};
use net::{MIN_RECV_BUFFER, Socket};

// recv_from has no timeout, so a broken socket would hang the test run.
// Receiving on a clone in another thread turns that into a failure instead.
fn recv_within(sock: &Socket, limit: Duration) -> (Vec<u8>, SocketAddr) {
    let clone = sock.try_clone().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = [0u8; MIN_RECV_BUFFER];
        let got = clone
            .recv_from(&mut buf)
            .map(|(n, from)| (buf[..n].to_vec(), from));
        let _ = tx.send(got);
    });
    rx.recv_timeout(limit)
        .expect("nothing arrived in time")
        .expect("recv_from failed")
}

fn exchange(a: &Socket, b: &Socket, to_a: SocketAddr, to_b: SocketAddr) {
    a.send_to(b"ping from a", to_b).unwrap();
    let (data, from) = recv_within(b, Duration::from_secs(2));
    assert_eq!(data, b"ping from a");
    assert_eq!(from.port(), a.local_port());
    assert_eq!(from.ip(), to_b.ip());

    b.send_to(b"pong from b", to_a).unwrap();
    let (data, from) = recv_within(a, Duration::from_secs(2));
    assert_eq!(data, b"pong from b");
    assert_eq!(from.port(), b.local_port());
}

#[test]
fn exchange_over_ipv4_loopback() {
    let a = Socket::bind(0).unwrap();
    let b = Socket::bind(0).unwrap();
    let to_a = SocketAddr::from((Ipv4Addr::LOCALHOST, a.local_port()));
    let to_b = SocketAddr::from((Ipv4Addr::LOCALHOST, b.local_port()));
    exchange(&a, &b, to_a, to_b);

    // On the dual-stack socket the source arrived as ::ffff:127.0.0.1; it
    // must come back out as a plain IPv4 address.
    a.send_to(b"x", to_b).unwrap();
    let (_, from) = recv_within(&b, Duration::from_secs(2));
    assert!(from.is_ipv4(), "{from}");
}

#[test]
fn exchange_over_ipv6_loopback() {
    if UdpSocket::bind("[::1]:0").is_err() {
        eprintln!("skipped: this pc has no ipv6 loopback");
        return;
    }
    let a = Socket::bind(0).unwrap();
    let b = Socket::bind(0).unwrap();
    if !a.has_ipv6() {
        eprintln!("skipped: the socket fell back to ipv4 only");
        return;
    }
    let to_a = SocketAddr::from((Ipv6Addr::LOCALHOST, a.local_port()));
    let to_b = SocketAddr::from((Ipv6Addr::LOCALHOST, b.local_port()));
    exchange(&a, &b, to_a, to_b);
}

#[test]
fn second_bind_on_the_same_port_is_in_use() {
    let first = Socket::bind(0).unwrap();
    let port = first.local_port();

    let err = Socket::bind(port).unwrap_err();
    assert!(err.is_in_use(), "{err:?}");
    assert_eq!(err.port, port);
    let holder = err.holder().expect("windows names who holds the port");
    assert!(holder.is_this_process(), "{holder:?}");
    assert_eq!(
        err.to_string(),
        format!(
            "could not bind udp port {port}: address in use, held by this process, pid {}",
            std::process::id()
        )
    );

    // One socket holds the port for both families.
    if first.has_ipv6() {
        assert!(UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).is_err());
        assert!(UdpSocket::bind((Ipv6Addr::UNSPECIFIED, port)).is_err());
    }
}

#[test]
fn wake_unblocks_a_blocked_receive() {
    let sock = Socket::bind(0).unwrap();
    let receiver = sock.try_clone().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = [0u8; MIN_RECV_BUFFER];
        let _ = tx.send(receiver.recv_from(&mut buf));
    });
    // Give the thread time to actually block in recv_from.
    thread::sleep(Duration::from_millis(100));

    let woken_at = Instant::now();
    sock.wake().unwrap();
    let (n, from) = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("receive thread was still blocked a second after wake")
        .unwrap();
    println!("wake took {:?}", woken_at.elapsed());
    assert_eq!(n, 0);
    assert!(from.ip().is_loopback(), "{from}");
    assert_eq!(from.port(), sock.local_port());
}

#[test]
fn port_unreachable_is_not_an_error() {
    let a = Socket::bind(0).unwrap();
    let b = Socket::bind(0).unwrap();
    let closed = {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        s.local_addr().unwrap()
    };
    // Loopback answers this with ICMP port-unreachable, which plain Windows
    // UDP turns into a failed receive.
    a.send_to(b"anyone there", closed).unwrap();
    thread::sleep(Duration::from_millis(50));

    b.send_to(
        b"real",
        SocketAddr::from((Ipv4Addr::LOCALHOST, a.local_port())),
    )
    .unwrap();
    let (data, _) = recv_within(&a, Duration::from_secs(2));
    assert_eq!(data, b"real");
}

// A datagram bigger than the receive buffer used to vanish without a trace.
#[test]
fn oversized_datagrams_are_dropped_and_counted() {
    let sock = Socket::bind(0).unwrap();
    let stats_view = sock.try_clone().unwrap();
    let to_sock = SocketAddr::from((Ipv4Addr::LOCALHOST, sock.local_port()));
    let stranger = UdpSocket::bind("127.0.0.1:0").unwrap();

    stranger
        .send_to(&[0x11; MIN_RECV_BUFFER + 100], to_sock)
        .unwrap();
    stranger.send_to(b"fits", to_sock).unwrap();
    let (data, _) = recv_within(&sock, Duration::from_secs(2));
    assert_eq!(data, b"fits");
    assert_eq!(stats_view.oversized_drops(), 1);

    // Exactly the minimum buffer still fits.
    stranger.send_to(&[0x11; MIN_RECV_BUFFER], to_sock).unwrap();
    let (data, _) = recv_within(&sock, Duration::from_secs(2));
    assert_eq!(data.len(), MIN_RECV_BUFFER);
    assert_eq!(sock.oversized_drops(), 1);
}

#[test]
fn pinned_ipv6_source_leaves_loopback_and_ipv4_alone() {
    let a = Socket::bind(0).unwrap();
    let b = Socket::bind(0).unwrap();
    let sender = a.try_clone().unwrap();
    // Not an address of this PC: if it were applied, the sends would fail.
    let not_ours: Ipv6Addr = "2001:db8::1".parse().unwrap();
    a.set_ipv6_source(Some(not_ours));
    assert_eq!(sender.ipv6_source(), Some(not_ours));

    let to_b = SocketAddr::from((Ipv4Addr::LOCALHOST, b.local_port()));
    sender.send_to(b"v4", to_b).unwrap();
    let (data, _) = recv_within(&b, Duration::from_secs(2));
    assert_eq!(data, b"v4");

    if a.has_ipv6() && UdpSocket::bind("[::1]:0").is_ok() {
        let to_b = SocketAddr::from((Ipv6Addr::LOCALHOST, b.local_port()));
        sender.send_to(b"v6", to_b).unwrap();
        let (data, from) = recv_within(&b, Duration::from_secs(2));
        assert_eq!(data, b"v6");
        assert_eq!(from.ip(), Ipv6Addr::LOCALHOST);
    }

    sender.set_ipv6_source(None);
    assert_eq!(a.ipv6_source(), None);
}

// Needs a global IPv6 address on this PC; sending to it goes over loopback
// but through the same source selection as a send to a friend.
#[test]
fn pinned_ipv6_source_is_used_for_global_targets() {
    let Some(stable) = local_addresses()
        .unwrap()
        .into_iter()
        .find(|a| a.kind == AddrKind::Ipv6Global)
        .and_then(|a| match a.ip {
            IpAddr::V6(v6) => Some(v6),
            IpAddr::V4(_) => None,
        })
    else {
        eprintln!("skipped: this pc has no global ipv6 address");
        return;
    };
    let a = Socket::bind(0).unwrap();
    let b = Socket::bind(0).unwrap();
    let to_b = SocketAddr::from((stable, b.local_port()));

    a.set_ipv6_source(Some(stable));
    a.send_to(b"from the stable address", to_b).unwrap();
    let (data, from) = recv_within(&b, Duration::from_secs(2));
    assert_eq!(data, b"from the stable address");
    assert_eq!(from, SocketAddr::from((stable, a.local_port())));

    let not_ours: Ipv6Addr = "2001:db8::1".parse().unwrap();
    a.set_ipv6_source(Some(not_ours));
    let err = a.send_to(b"x", to_b).unwrap_err();
    assert!(err.to_string().contains("2001:db8::1"), "{err}");
}
