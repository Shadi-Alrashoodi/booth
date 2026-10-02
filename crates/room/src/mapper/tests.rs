use std::io::{Read, Write};
use std::net::{Ipv6Addr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;

const LOCALHOST: Ipv4Addr = Ipv4Addr::LOCALHOST;
// TEST-NET-3, public as far as anything here cares.
const OUTSIDE: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 7);
// Tests that leave a mapper thread running past its room use ports of
// their own, so no other test waits for it.
const PORT: u16 = 41105;
const GATEWAY: Gateway = Gateway {
    ip: LOCALHOST,
    local: LOCALHOST,
};

fn target(pcp: SocketAddr, ssdp: SocketAddr) -> Target {
    Target {
        gateway: GATEWAY,
        pcp,
        ssdp,
        least_lifetime: 1,
        // Only the test of it waits for a second try after a loss.
        again: [Duration::from_secs(3600); 3],
    }
}

// A port nothing listens on, which loopback answers with port unreachable.
fn closed_port() -> SocketAddr {
    let socket = UdpSocket::bind((LOCALHOST, 0)).unwrap();
    socket.local_addr().unwrap()
}

fn start(target: Target, log: Log) -> (Mapper, Receiver<Report>) {
    start_on(PORT, target, log)
}

fn start_on(port: u16, target: Target, log: Log) -> (Mapper, Receiver<Report>) {
    let (tx, rx) = crossbeam_channel::unbounded();
    let mapper = Mapper::start(target, port, log, move |report| {
        let _ = tx.send(report);
    })
    .unwrap();
    (mapper, rx)
}

fn mapped(protocol: Protocol, lifetime: u32) -> Report {
    mapped_on(PORT, protocol, lifetime)
}

fn mapped_on(port: u16, protocol: Protocol, lifetime: u32) -> Report {
    Report::Mapped {
        protocol,
        external: SocketAddrV4::new(OUTSIDE, port),
        lifetime,
    }
}

#[derive(Clone, Copy)]
enum Speaks {
    // Grants `lifetime`, answering after `delay`.
    Pcp { lifetime: u32, delay: Duration },
    // Answers PCP with NAT-PMP's own "unsupported version".
    NatPmp,
    // Refuses PCP as a router does for a port it maps for another nonce,
    // and grants NAT-PMP.
    PcpForAnotherNonce,
    // Grants the first request and then falls silent, as a router does
    // when it restarts with its mapping service off.
    PcpOnce { lifetime: u32 },
    Nothing,
}

// A router's PCP and NAT-PMP port on loopback. Every request is kept.
struct FakeRouter {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Vec<u8>>>>,
    // Restarting: requests are kept and nothing is answered.
    down: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FakeRouter {
    fn start(speaks: Speaks) -> FakeRouter {
        // Blocking, with no timeout: Windows can lose a datagram that lands
        // just as a receive times out. Drop wakes it with an empty one.
        let socket = UdpSocket::bind((LOCALHOST, 0)).unwrap();
        let addr = socket.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let down = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = thread::spawn({
            let (seen, down, stop) = (Arc::clone(&seen), Arc::clone(&down), Arc::clone(&stop));
            move || {
                let mut buf = [0u8; 1500];
                loop {
                    let got = socket.recv_from(&mut buf);
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                    let Ok((len, from)) = got else {
                        continue;
                    };
                    let request = buf[..len].to_vec();
                    let count = {
                        let mut seen = lock(&seen);
                        seen.push(request.clone());
                        seen.len()
                    };
                    if down.load(Ordering::Acquire) {
                        continue;
                    }
                    let speaks = match speaks {
                        Speaks::PcpOnce { lifetime } if count == 1 => Speaks::Pcp {
                            lifetime,
                            delay: Duration::ZERO,
                        },
                        Speaks::PcpOnce { .. } => Speaks::Nothing,
                        other => other,
                    };
                    let Some((answer, delay)) = answer(speaks, &request) else {
                        continue;
                    };
                    // Each on its own, so one slow answer does not hold up
                    // the next request.
                    let socket = socket.try_clone().unwrap();
                    thread::spawn(move || {
                        thread::sleep(delay);
                        let _ = socket.send_to(&answer, from);
                    });
                }
            }
        });
        FakeRouter {
            addr,
            seen,
            down,
            stop,
            thread: Some(thread),
        }
    }

    fn requests(&self) -> Vec<Vec<u8>> {
        lock(&self.seen).clone()
    }

    fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::Release);
    }
}

impl Drop for FakeRouter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let waker = UdpSocket::bind((LOCALHOST, 0)).unwrap();
        let _ = waker.send_to(&[], self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

const EPOCH: [u8; 4] = 1000u32.to_be_bytes();

fn answer(speaks: Speaks, request: &[u8]) -> Option<(Vec<u8>, Duration)> {
    match (speaks, request.first()) {
        (Speaks::Pcp { lifetime, delay }, Some(2)) if request.len() == pcp::MAP_LEN => {
            Some((pcp_answer(request, 0, lifetime), delay))
        }
        (Speaks::PcpForAnotherNonce, Some(2)) if request.len() == pcp::MAP_LEN => Some((
            pcp_answer(request, ResultCode::NOT_AUTHORIZED.0, 0),
            Duration::ZERO,
        )),
        (Speaks::NatPmp, Some(2)) => {
            let mut out = vec![0, 0x80 | request[1], 0, 1];
            out.extend_from_slice(&EPOCH);
            Some((out, Duration::ZERO))
        }
        (Speaks::NatPmp | Speaks::PcpForAnotherNonce, Some(0)) if request == [0, 0] => {
            let mut out = vec![0, 128, 0, 0];
            out.extend_from_slice(&EPOCH);
            out.extend_from_slice(&OUTSIDE.octets());
            Some((out, Duration::ZERO))
        }
        (Speaks::NatPmp | Speaks::PcpForAnotherNonce, Some(0))
            if request.len() == 12 && request[1] == 1 =>
        {
            let mut out = vec![0, 129, 0, 0];
            out.extend_from_slice(&EPOCH);
            out.extend_from_slice(&request[4..6]);
            out.extend_from_slice(&request[6..8]);
            out.extend_from_slice(&request[8..12]);
            Some((out, Duration::ZERO))
        }
        _ => None,
    }
}

// The request turned into its answer: the external port is the internal
// one, on OUTSIDE.
fn pcp_answer(request: &[u8], result: u8, lifetime: u32) -> Vec<u8> {
    let asked = u32::from_be_bytes(request[4..8].try_into().unwrap());
    let mut out = request.to_vec();
    out[1] |= 0x80;
    out[3] = result;
    let granted = if asked == 0 { 0 } else { lifetime };
    out[4..8].copy_from_slice(&granted.to_be_bytes());
    out[8..12].copy_from_slice(&EPOCH);
    out[12..24].fill(0);
    let internal_port = [request[40], request[41]];
    out[42..44].copy_from_slice(&internal_port);
    out[44..60].copy_from_slice(&OUTSIDE.to_ipv6_mapped().octets());
    out
}

fn lifetime_asked(request: &[u8]) -> u32 {
    let at = if request[0] == 2 { 4 } else { 8 };
    u32::from_be_bytes(request[at..at + 4].try_into().unwrap())
}

fn has_line(lines: &[String], want: &str) -> bool {
    lines.iter().any(|line| line == want)
}

#[test]
fn pcp_renewed_and_deleted() {
    let router = FakeRouter::start(Speaks::Pcp {
        lifetime: 2,
        delay: Duration::ZERO,
    });
    let (log, captured) = Log::capture(1024);
    let (mut mapper, reports) = start(target(router.addr, closed_port()), log);
    let wait = Duration::from_secs(3);
    assert_eq!(reports.recv_timeout(wait), Ok(mapped(Protocol::Pcp, 2)));
    // Half of 2 s.
    let asked_at = Instant::now();
    assert_eq!(reports.recv_timeout(wait), Ok(mapped(Protocol::Pcp, 2)));
    assert!(asked_at.elapsed() >= Duration::from_millis(900));

    mapper.close();
    assert!(mapper.wait(Instant::now() + wait));
    let requests = router.requests();
    assert_eq!(requests.len(), 3, "map, renew, delete");
    let nonce = &requests[0][24..36];
    assert!(requests.iter().all(|r| &r[24..36] == nonce), "one nonce");
    assert_eq!(lifetime_asked(&requests[0]), LIFETIME);
    // The renewal suggests what was granted.
    assert_eq!(&requests[1][42..44], &PORT.to_be_bytes());
    assert_eq!(&requests[1][44..60], &OUTSIDE.to_ipv6_mapped().octets());
    assert_eq!(lifetime_asked(&requests[2]), 0);

    let lines = captured.lines();
    assert!(
        has_line(&lines, "pcp: the router opened 203.0.113.7:41105 for 2 s"),
        "{lines:#?}"
    );
    assert!(has_line(&lines, "pcp: renewed 203.0.113.7:41105 for 2 s"));
    assert!(has_line(&lines, "pcp: the router deleted the mapping"));
    assert!(reports.try_recv().is_err());
}

#[test]
fn nat_pmp_when_no_pcp() {
    let router = FakeRouter::start(Speaks::NatPmp);
    let (log, captured) = Log::capture(1024);
    let (mut mapper, reports) = start(target(router.addr, closed_port()), log);
    let wait = Duration::from_secs(3);
    assert_eq!(
        reports.recv_timeout(wait),
        Ok(mapped(Protocol::NatPmp, LIFETIME))
    );
    mapper.close();
    assert!(mapper.wait(Instant::now() + wait));

    let requests = router.requests();
    let shapes: Vec<(u8, usize)> = requests.iter().map(|r| (r[0], r.len())).collect();
    assert_eq!(shapes, [(2, pcp::MAP_LEN), (0, 2), (0, 12), (0, 12)]);
    // The deletion: port 0 and lifetime 0 for our internal port.
    assert_eq!(&requests[3][4..], &[0xA0, 0x91, 0, 0, 0, 0, 0, 0]);
    let lines = captured.lines();
    assert!(
        has_line(&lines, "pcp: the router speaks nat-pmp, not pcp"),
        "{lines:#?}"
    );
    assert!(has_line(&lines, "nat-pmp: the router deleted the mapping"));
}

// A run that ended without deleting its PCP mapping leaves it on the router
// for up to two hours under a nonce nobody has any more.
#[test]
fn pcp_other_nonce_falls_to_nat_pmp() {
    let router = FakeRouter::start(Speaks::PcpForAnotherNonce);
    let (log, captured) = Log::capture(1024);
    let (mut mapper, reports) = start(target(router.addr, closed_port()), log);
    let wait = Duration::from_secs(3);
    assert_eq!(
        reports.recv_timeout(wait),
        Ok(mapped(Protocol::NatPmp, LIFETIME))
    );
    mapper.close();
    assert!(mapper.wait(Instant::now() + wait));
    let lines = captured.lines();
    assert!(
        has_line(
            &lines,
            "pcp: the router refused the pcp mapping: NOT_AUTHORIZED (2)"
        ),
        "{lines:#?}"
    );
}

#[test]
fn a_grant_too_short_to_keep_is_not_used() {
    let router = FakeRouter::start(Speaks::Pcp {
        lifetime: 60,
        delay: Duration::ZERO,
    });
    let (log, captured) = Log::capture(1024);
    let strict = Target {
        least_lifetime: LEAST_LIFETIME,
        ..target(router.addr, closed_port())
    };
    let (mut mapper, reports) = start(strict, log);
    assert_eq!(
        reports.recv_timeout(Duration::from_secs(5)),
        Ok(Report::Unmapped { wan: None })
    );
    mapper.close();
    assert!(mapper.wait(Instant::now() + Duration::from_secs(1)));
    assert_eq!(router.requests().len(), 1, "no nat-pmp, and no renewals");
    let lines = captured.lines();
    assert!(
        has_line(
            &lines,
            "pcp: the router opened 203.0.113.7:41105 for 60 s, less than the 120 s the room takes; not used, it runs out on its own"
        ),
        "{lines:#?}"
    );
    assert!(has_line(
        &lines,
        "nat-pmp: not asked, the router answered pcp"
    ));
}

#[test]
fn the_room_can_ask_for_a_renewal_now() {
    let router = FakeRouter::start(Speaks::Pcp {
        lifetime: LIFETIME,
        delay: Duration::ZERO,
    });
    let (log, captured) = Log::capture(1024);
    let (mut mapper, reports) = start(target(router.addr, closed_port()), log);
    let wait = Duration::from_secs(3);
    assert_eq!(
        reports.recv_timeout(wait),
        Ok(mapped(Protocol::Pcp, LIFETIME))
    );
    mapper.renewer().send(()).unwrap();
    assert_eq!(
        reports.recv_timeout(wait),
        Ok(mapped(Protocol::Pcp, LIFETIME))
    );
    mapper.close();
    assert!(mapper.wait(Instant::now() + wait));
    assert_eq!(router.requests().len(), 3, "map, renew, delete");
    assert!(has_line(
        &captured.lines(),
        "port mapping: stun saw the outside address change, renewing now"
    ));
}

// The PCP client address in a map request.
fn client_in(request: &[u8]) -> Ipv4Addr {
    let octets: [u8; 16] = request[8..24].try_into().unwrap();
    Ipv6Addr::from(octets).to_ipv4_mapped().unwrap()
}

// The room's outside address changed: the mapping is deleted, not renewed,
// and the ladder runs again, from the address the router gave this PC when
// it came back.
#[test]
fn the_room_can_have_the_mapping_made_again() {
    let router = FakeRouter::start(Speaks::Pcp {
        lifetime: LIFETIME,
        delay: Duration::ZERO,
    });
    let (log, captured) = Log::capture(1024);
    let (mut mapper, reports) = start(target(router.addr, closed_port()), log);
    let wait = Duration::from_secs(3);
    assert_eq!(
        reports.recv_timeout(wait),
        Ok(mapped(Protocol::Pcp, LIFETIME))
    );
    let moved = Ipv4Addr::new(127, 0, 0, 2);
    mapper
        .remapper()
        .send(Gateway {
            local: moved,
            ..GATEWAY
        })
        .unwrap();
    assert_eq!(
        reports.recv_timeout(wait),
        Ok(mapped(Protocol::Pcp, LIFETIME))
    );
    mapper.close();
    assert!(mapper.wait(Instant::now() + wait));
    let requests = router.requests();
    let asked: Vec<(u32, Ipv4Addr)> = requests
        .iter()
        .map(|request| (lifetime_asked(request), client_in(request)))
        .collect();
    assert_eq!(
        asked,
        [
            (LIFETIME, LOCALHOST),
            (0, LOCALHOST),
            (LIFETIME, moved),
            (0, moved)
        ],
        "map, delete, map, delete"
    );
    let lines = captured.lines();
    assert!(
        has_line(
            &lines,
            "port mapping: this pc's outside address changed; deleting the mapping and asking the router again, since some routers keep listing a mapping that stopped working"
        ),
        "{lines:#?}"
    );
    assert!(has_line(
        &lines,
        "pcp: deleting the mapping for 203.0.113.7:41105 before asking again"
    ));
    assert!(has_line(
        &lines,
        "port mapping: the router to ask is 127.0.0.1 now, this pc is 127.0.0.2 to it"
    ));
}

// STUN shows the new address while the router is still starting the
// service that answers mapping asks, so the ladder after the deletion finds
// nothing. The mapper asks again a little later instead of leaving the
// room without a mapping.
#[test]
fn lost_mapping_asked_again() {
    let router = FakeRouter::start(Speaks::Pcp {
        lifetime: LIFETIME,
        delay: Duration::ZERO,
    });
    let (log, captured) = Log::capture(1024);
    let quick = Target {
        again: [Duration::from_millis(300); 3],
        ..target(router.addr, closed_port())
    };
    let (mut mapper, reports) = start(quick, log);
    let wait = Duration::from_secs(3);
    assert_eq!(
        reports.recv_timeout(wait),
        Ok(mapped(Protocol::Pcp, LIFETIME))
    );

    router.set_down(true);
    mapper.remapper().send(GATEWAY).unwrap();
    // The deletion, PCP and NAT-PMP 750 ms each, then the SSDP search.
    assert_eq!(
        reports.recv_timeout(Duration::from_secs(8)),
        Ok(Report::Unmapped { wan: None })
    );
    router.set_down(false);
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match reports.recv_deadline(deadline) {
            Ok(Report::Unmapped { .. }) => {}
            got => {
                assert_eq!(got, Ok(mapped(Protocol::Pcp, LIFETIME)));
                break;
            }
        }
    }
    mapper.close();
    assert!(mapper.wait(Instant::now() + wait));
    let last = router.requests().last().cloned().unwrap();
    assert_eq!((last[0], lifetime_asked(&last)), (2, 0), "deleted at close");
    let lines = captured.lines();
    assert!(
        has_line(&lines, "port mapping: asking the router again in 0.3 s"),
        "{lines:#?}"
    );
}

// External port, internal client, internal port, description.
type Table = Mutex<Vec<(u16, String, u16, String)>>;

// A description, the four SOAP calls a mapping needs, and a table of
// entries the test can change behind the mapper's back, on loopback.
struct FakeIgd {
    ssdp: SocketAddr,
    actions: Arc<Mutex<Vec<String>>>,
    table: Arc<Table>,
}

impl FakeIgd {
    fn start() -> FakeIgd {
        let listener = TcpListener::bind((LOCALHOST, 0)).unwrap();
        let web = listener.local_addr().unwrap();
        let actions = Arc::new(Mutex::new(Vec::new()));
        let table = Arc::new(Mutex::new(Vec::new()));
        thread::spawn({
            let (actions, table) = (Arc::clone(&actions), Arc::clone(&table));
            move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { return };
                    let request = read_request(&mut stream);
                    let reply = igd_answer(&request, &actions, &table);
                    let _ = stream.write_all(reply.as_bytes());
                }
            }
        });

        let ssdp = UdpSocket::bind((LOCALHOST, 0)).unwrap();
        let ssdp_addr = ssdp.local_addr().unwrap();
        thread::spawn(move || {
            let mut buf = [0u8; 2048];
            let answer = format!(
                "HTTP/1.1 200 OK\r\nST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\nLOCATION: http://{web}/rootDesc.xml\r\n\r\n"
            );
            while let Ok((_, from)) = ssdp.recv_from(&mut buf) {
                let _ = ssdp.send_to(answer.as_bytes(), from);
            }
        });
        FakeIgd {
            ssdp: ssdp_addr,
            actions,
            table,
        }
    }

    fn actions(&self) -> Vec<String> {
        lock(&self.actions).clone()
    }
}

fn read_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    while let Ok(len) = stream.read(&mut buf) {
        if len == 0 {
            break;
        }
        data.extend_from_slice(&buf[..len]);
        let text = String::from_utf8_lossy(&data);
        if let Some(head) = text.find("\r\n\r\n") {
            let length = text[..head]
                .lines()
                .find_map(|line| line.strip_prefix("Content-Length: "))
                .map_or(0, |value| value.trim().parse().unwrap());
            if data.len() >= head + 4 + length {
                break;
            }
        }
    }
    String::from_utf8_lossy(&data).into_owned()
}

// The text of <name>...</name> in a request the mapper sent.
fn arg(request: &str, name: &str) -> String {
    let open = format!("<{name}>");
    let Some(start) = request.find(&open).map(|at| at + open.len()) else {
        return String::new();
    };
    let end = request[start..]
        .find(&format!("</{name}>"))
        .map_or(start, |at| start + at);
    request[start..end].to_owned()
}

fn igd_answer(request: &str, actions: &Mutex<Vec<String>>, table: &Table) -> String {
    const URN: &str = "urn:schemas-upnp-org:service:WANIPConnection:1";
    let (status, body) = if request.starts_with("GET ") {
        (
            "200 OK",
            format!(
                "<?xml version=\"1.0\"?><root><device><serviceList><service><serviceType>{URN}</serviceType><controlURL>/ctl/IPConn</controlURL></service></serviceList></device></root>"
            ),
        )
    } else {
        let action = request
            .lines()
            .find_map(|line| line.strip_prefix("SOAPAction: "))
            .and_then(|value| value.trim_matches('"').split('#').nth(1))
            .unwrap_or_default()
            .to_owned();
        let port: u16 = arg(request, "NewExternalPort").parse().unwrap_or(0);
        let mut table = lock(table);
        let found = table.iter().position(|(on, ..)| *on == port);
        let values = match action.as_str() {
            "GetExternalIPAddress" => Some(format!(
                "<NewExternalIPAddress>{OUTSIDE}</NewExternalIPAddress>"
            )),
            "GetSpecificPortMappingEntry" => found.map(|at| {
                let (_, client, internal_port, name) = &table[at];
                format!(
                    "<NewInternalPort>{internal_port}</NewInternalPort><NewInternalClient>{client}</NewInternalClient><NewEnabled>1</NewEnabled><NewPortMappingDescription>{name}</NewPortMappingDescription><NewLeaseDuration>3600</NewLeaseDuration>"
                )
            }),
            "AddPortMapping" => {
                let entry = (
                    port,
                    arg(request, "NewInternalClient"),
                    arg(request, "NewInternalPort").parse().unwrap_or(0),
                    arg(request, "NewPortMappingDescription"),
                );
                match found {
                    Some(at) => table[at] = entry,
                    None => table.push(entry),
                }
                Some(String::new())
            }
            "DeletePortMapping" => found.map(|at| {
                table.remove(at);
                String::new()
            }),
            _ => None,
        };
        lock(actions).push(action.clone());
        match values {
            Some(values) => (
                "200 OK",
                format!(
                    "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:{action}Response xmlns:u=\"{URN}\">{values}</u:{action}Response></s:Body></s:Envelope>"
                ),
            ),
            None => (
                "500 Internal Server Error",
                String::from(
                    "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><s:Fault><detail><UPnPError><errorCode>714</errorCode></UPnPError></detail></s:Fault></s:Body></s:Envelope>",
                ),
            ),
        }
    };
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[test]
fn upnp_when_pcp_port_closed() {
    let igd = FakeIgd::start();
    let (log, captured) = Log::capture(1024);
    let (mut mapper, reports) = start(target(closed_port(), igd.ssdp), log);
    let wait = Duration::from_secs(5);
    assert_eq!(
        reports.recv_timeout(wait),
        Ok(mapped(Protocol::Upnp, upnp::LEASE))
    );
    mapper.close();
    assert!(mapper.wait(Instant::now() + wait));
    assert_eq!(
        igd.actions(),
        [
            "GetExternalIPAddress",
            "GetSpecificPortMappingEntry",
            "AddPortMapping",
            "GetSpecificPortMappingEntry",
            "DeletePortMapping"
        ]
    );
    assert!(lock(&igd.table).is_empty());
    let lines = captured.lines();
    assert!(
        has_line(
            &lines,
            "pcp: the router answered port unreachable: it has no pcp or nat-pmp service"
        ),
        "{lines:#?}"
    );
    assert!(has_line(
        &lines,
        "nat-pmp: not asked, nothing listens on its port"
    ));
    assert!(!lines.iter().any(|line| line.starts_with("nat-pmp: asking")));
    assert!(has_line(
        &lines,
        "upnp: the router opened 203.0.113.7:41105 for 7200 s"
    ));
    assert!(has_line(
        &lines,
        "upnp: the router deleted the mapping for udp 41105"
    ));
}

// The router deletes by port alone. A lease that ran out, or a router that
// restarted, may have given the port to another device by the time the
// room closes.
#[test]
fn foreign_port_left_at_close() {
    let igd = FakeIgd::start();
    let (log, captured) = Log::capture(1024);
    let (mut mapper, reports) = start(target(closed_port(), igd.ssdp), log);
    let wait = Duration::from_secs(5);
    assert_eq!(
        reports.recv_timeout(wait),
        Ok(mapped(Protocol::Upnp, upnp::LEASE))
    );
    *lock(&igd.table) = vec![(
        PORT,
        String::from("192.168.1.50"),
        PORT,
        String::from("console"),
    )];
    mapper.close();
    assert!(mapper.wait(Instant::now() + wait));
    assert!(!igd.actions().contains(&String::from("DeletePortMapping")));
    assert_eq!(lock(&igd.table).len(), 1);
    assert!(has_line(
        &captured.lines(),
        "upnp: udp 41105 forwards to 192.168.1.50:41105 (\"console\") now, not the room's, left in place"
    ));
}

// Some routers keep no names, so a renewal reads the room's own entry as a
// forward someone else made. It is still the room's to delete.
#[test]
fn nameless_entry_still_deleted() {
    let igd = FakeIgd::start();
    let (log, captured) = Log::capture(1024);
    let (mut mapper, reports) = start(target(closed_port(), igd.ssdp), log);
    let wait = Duration::from_secs(5);
    assert_eq!(
        reports.recv_timeout(wait),
        Ok(mapped(Protocol::Upnp, upnp::LEASE))
    );
    lock(&igd.table)[0].3.clear();
    mapper.renewer().send(()).unwrap();
    assert!(matches!(
        reports.recv_timeout(wait),
        Ok(Report::Mapped { .. })
    ));
    mapper.close();
    assert!(mapper.wait(Instant::now() + wait));
    assert_eq!(
        igd.actions().last().map(String::as_str),
        Some("DeletePortMapping")
    );
    assert!(lock(&igd.table).is_empty());
    let lines = captured.lines();
    assert!(
        has_line(&lines, "upnp: the router deleted the mapping for udp 41105"),
        "{lines:#?}"
    );
}

// The router gave the room's port to another device, so the renewal moves
// to a random one. The old port is looked at again at close and left.
#[test]
fn moved_renewal_keeps_old_port() {
    let igd = FakeIgd::start();
    let (log, captured) = Log::capture(1024);
    let (mut mapper, reports) = start(target(closed_port(), igd.ssdp), log);
    let wait = Duration::from_secs(5);
    assert_eq!(
        reports.recv_timeout(wait),
        Ok(mapped(Protocol::Upnp, upnp::LEASE))
    );
    *lock(&igd.table) = vec![(
        PORT,
        String::from("192.168.1.50"),
        PORT,
        String::from("console"),
    )];
    mapper.renewer().send(()).unwrap();
    let Ok(Report::Mapped { external, .. }) = reports.recv_timeout(wait) else {
        panic!("no renewal");
    };
    assert_ne!(external.port(), PORT);
    mapper.close();
    assert!(mapper.wait(Instant::now() + wait));

    let table = lock(&igd.table).clone();
    assert_eq!(
        table,
        [(
            PORT,
            String::from("192.168.1.50"),
            PORT,
            String::from("console")
        )]
    );
    let lines = captured.lines();
    assert!(
        has_line(
            &lines,
            "upnp: udp 41105 may still be the room's on the router, deleted when the room closes"
        ),
        "{lines:#?}"
    );
    assert!(has_line(
        &lines,
        &format!(
            "upnp: the router deleted the mapping for udp {}",
            external.port()
        )
    ));
    assert!(has_line(
        &lines,
        "upnp: udp 41105 forwards to 192.168.1.50:41105 (\"console\") now, not the room's, left in place"
    ));
}

#[test]
fn silent_router() {
    let silent = FakeRouter::start(Speaks::Nothing);
    let ssdp = FakeRouter::start(Speaks::Nothing);
    let (mut mapper, reports) = start(target(silent.addr, ssdp.addr), Log::off());
    // PCP and NAT-PMP each wait 750 ms, then the SSDP search 1 s.
    assert_eq!(
        reports.recv_timeout(Duration::from_secs(5)),
        Ok(Report::Unmapped { wan: None })
    );
    // With nothing held there is nothing to wait for.
    let closed = Instant::now();
    mapper.close();
    assert!(mapper.wait(closed + Duration::from_secs(1)));
    assert!(closed.elapsed() < Duration::from_millis(100));
    let tries: Vec<(u8, usize)> = silent.requests().iter().map(|r| (r[0], r.len())).collect();
    assert_eq!(
        tries,
        [(2, pcp::MAP_LEN), (2, pcp::MAP_LEN), (0, 2), (0, 2)]
    );
    // The search went out twice, once more at 300 ms.
    assert_eq!(ssdp.requests().len(), 4);
}

// Leaving halfway through the ladder must not read as a router that cannot
// map.
#[test]
fn close_during_ladder() {
    let silent = FakeRouter::start(Speaks::Nothing);
    let (log, captured) = Log::capture(1024);
    let (mut mapper, _reports) = start(target(silent.addr, closed_port()), log);
    thread::sleep(Duration::from_millis(100));
    mapper.close();
    assert!(mapper.wait(Instant::now() + Duration::from_secs(3)));
    let lines = captured.lines();
    assert!(
        has_line(
            &lines,
            "port mapping: room closed before nat-pmp and upnp were asked"
        ),
        "{lines:#?}"
    );
    assert!(has_line(
        &lines,
        "port mapping: room closed, nothing to delete"
    ));
    assert!(!has_line(&lines, "port mapping: the router opened no port"));
}

// leave() waits DELETE_WAIT at most. A slower deletion still happens, and
// its lines still reach the file.
#[test]
fn slow_deletion_after_leave() {
    const OWN_PORT: u16 = 41106;
    let path = std::env::temp_dir().join(format!("booth-mapper-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let (log, writer) = crate::log::open(Some(&path), "host").unwrap();
    let router = FakeRouter::start(Speaks::Pcp {
        lifetime: LIFETIME,
        delay: Duration::from_millis(400),
    });
    let (mut mapper, reports) = start_on(OWN_PORT, target(router.addr, closed_port()), log);
    assert_eq!(
        reports.recv_timeout(Duration::from_secs(3)),
        Ok(mapped_on(OWN_PORT, Protocol::Pcp, LIFETIME))
    );

    let closed = Instant::now();
    mapper.close();
    assert!(!mapper.wait(closed + Duration::from_millis(150)));
    assert!(closed.elapsed() < Duration::from_millis(300));
    mapper.hand_over(writer.expect("a log file was asked for"));
    drop(mapper);

    let deadline = Instant::now() + Duration::from_secs(5);
    let written = loop {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        if text.contains("pcp: the router deleted the mapping") || Instant::now() >= deadline {
            break text;
        }
        thread::sleep(Duration::from_millis(20));
    };
    let _ = std::fs::remove_file(&path);
    assert!(
        written.contains("pcp: the router deleted the mapping"),
        "{written}"
    );
    assert_eq!(lifetime_asked(router.requests().last().unwrap()), 0);
}

// Closing the window returns from main, which ends the process and every
// thread in it, a deletion halfway or not.
#[test]
fn process_waits_for_deletions() {
    const OWN_PORT: u16 = 41107;
    let router = FakeRouter::start(Speaks::Pcp {
        lifetime: LIFETIME,
        delay: Duration::from_millis(400),
    });
    let (mut mapper, reports) = start_on(OWN_PORT, target(router.addr, closed_port()), Log::off());
    assert_eq!(
        reports.recv_timeout(Duration::from_secs(3)),
        Ok(mapped_on(OWN_PORT, Protocol::Pcp, LIFETIME))
    );
    mapper.close();
    assert!(!mapper.wait(Instant::now() + Duration::from_millis(150)));
    drop(mapper);
    assert!(finish(Duration::from_secs(3)));
    assert_eq!(lifetime_asked(router.requests().last().unwrap()), 0);
}

// Leave during a slow deletion and host again at once, on the same port:
// the old thread's deletion would remove the new room's mapping.
#[test]
fn new_room_waits_for_old_deletion() {
    const OWN_PORT: u16 = 41108;
    let slow = FakeRouter::start(Speaks::Pcp {
        lifetime: LIFETIME,
        delay: Duration::from_millis(400),
    });
    let (mut old, reports) = start_on(OWN_PORT, target(slow.addr, closed_port()), Log::off());
    assert_eq!(
        reports.recv_timeout(Duration::from_secs(3)),
        Ok(mapped_on(OWN_PORT, Protocol::Pcp, LIFETIME))
    );
    old.close();
    assert!(!old.wait(Instant::now() + Duration::from_millis(150)));
    drop(old);

    let fast = FakeRouter::start(Speaks::Pcp {
        lifetime: LIFETIME,
        delay: Duration::ZERO,
    });
    let (log, captured) = Log::capture(1024);
    let (mut new, reports) = start_on(OWN_PORT, target(fast.addr, closed_port()), log);
    assert_eq!(
        reports.recv_timeout(Duration::from_secs(3)),
        Ok(mapped_on(OWN_PORT, Protocol::Pcp, LIFETIME))
    );
    // By the time the new room asked, the old deletion was answered.
    assert_eq!(lifetime_asked(slow.requests().last().unwrap()), 0);
    let lines = captured.lines();
    assert!(
        has_line(
            &lines,
            "port mapping: the last room on udp 41108 is still deleting its mapping, waiting for it first"
        ),
        "{lines:#?}"
    );
    assert!(has_line(
        &lines,
        "port mapping: the last room's thread is done"
    ));
    new.close();
    assert!(new.wait(Instant::now() + Duration::from_secs(3)));
}

#[test]
fn failed_renewal_then_unmapped() {
    let router = FakeRouter::start(Speaks::PcpOnce { lifetime: 2 });
    let (log, captured) = Log::capture(1024);
    let (mut mapper, reports) = start(target(router.addr, closed_port()), log);
    assert_eq!(
        reports.recv_timeout(Duration::from_secs(3)),
        Ok(mapped(Protocol::Pcp, 2))
    );
    // The renewal after 1 s and the ladder after it: PCP and NAT-PMP 750 ms
    // each, the SSDP search 1 s.
    assert_eq!(
        reports.recv_timeout(Duration::from_secs(6)),
        Ok(Report::Unmapped { wan: None })
    );
    mapper.close();
    assert!(mapper.wait(Instant::now() + Duration::from_secs(3)));

    // The first mapping may still be on the router, so it is deleted too.
    let last = router.requests().last().cloned().unwrap();
    assert_eq!((last[0], lifetime_asked(&last)), (2, 0));
    let lines = captured.lines();
    assert!(
        lines.iter().any(
            |line| line.starts_with("pcp: the renewal failed: no pcp answer to 2 requests in ")
        ),
        "{lines:#?}"
    );
    assert!(has_line(
        &lines,
        "port mapping: room closed, deleting an older pcp mapping"
    ));
}

// The whole way round: the mapper's report reaches the host through the
// timer thread, the invite carries it, and leaving deletes it.
#[test]
fn hosted_room_maps_and_unmaps() {
    use crate::config::Timers;
    use crate::host::{Host, HostSetup};
    use crate::invites::Local;
    use crate::known::{DeviceBook, KnownDevices};
    use crate::socket::Socket;
    use crate::threads::{Outlets, Side, Threads, VoiceStart};
    use crate::view::RouterState;
    use keys::Identity;

    let router = FakeRouter::start(Speaks::Pcp {
        lifetime: LIFETIME,
        delay: Duration::ZERO,
    });
    let socket = Socket::bind(0, Log::off()).unwrap();
    let port = socket.local_port();
    let voice = crate::talk::Shared::new(&crate::testing::fake_voice(), None, true);
    let (speaker_in, speaker_out) = crossbeam_channel::unbounded();
    let host = Host::new(HostSetup {
        identity: Arc::new(Identity::generate()),
        name: "Mara".to_owned(),
        room_name: String::new(),
        timers: Timers::default(),
        local: Local::Fixed(Vec::new()),
        port,
        has_ipv6: socket.has_ipv6(),
        router: Some(Gateway {
            ip: LOCALHOST,
            local: LOCALHOST,
        }),
        punch_loopback: false,
        address_name: None,
        lookup: crate::config::Lookup::default(),
        devices: DeviceBook::new(KnownDevices::default(), true),
        list_problem: None,
        initiations_read: Arc::default(),
        voice: Arc::clone(&voice),
        speaker: speaker_in,
        screen: crate::testing::quiet_screen(),
        now: Instant::now(),
        log: Log::off(),
    });
    let clock = host.clock();
    let (log, captured) = Log::capture(1024);
    // Nobody joins, so nothing is read or saved there.
    let dir = std::env::temp_dir().join(format!("booth-mapper-room-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (_, turn) = crate::known::room_devices(&dir);
    let outlets = Outlets {
        notify: Arc::new(|| {}),
        log,
        writer: None,
        turn,
    };
    let mut threads = Threads::start(
        Side::Host(Box::new(host)),
        Arc::new(socket),
        Vec::new(),
        Some(target(router.addr, closed_port())),
        outlets,
        VoiceStart {
            config: crate::testing::fake_voice(),
            speaker: crate::talk::Speaker::new(speaker_out, Arc::clone(&voice), clock),
            shared: voice,
            clock,
        },
        crate::config::VideoConfig {
            source: crate::config::VideoSource::Hooks,
            ..crate::config::VideoConfig::default()
        },
    )
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(3);
    let invite = loop {
        let view = threads.view();
        if let Some(invite) = view.invite.clone().filter(|invite| !invite.code.is_empty()) {
            break invite;
        }
        assert!(Instant::now() < deadline, "no invite: {view:#?}");
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(invite.router, RouterState::Mapped);
    let decoded = invite::Invite::decode(&invite.code).unwrap();
    assert!(decoded.mapped);
    let external = SocketAddr::V4(SocketAddrV4::new(OUTSIDE, port));
    assert!(decoded.candidates.iter().any(|c| c.addr == external));

    let left = Instant::now();
    threads.stop();
    assert!(left.elapsed() < Duration::from_millis(300));
    assert_eq!(lifetime_asked(router.requests().last().unwrap()), 0);
    assert!(has_line(
        &captured.lines(),
        "pcp: the router deleted the mapping"
    ));
    let _ = std::fs::remove_dir_all(&dir);
}
