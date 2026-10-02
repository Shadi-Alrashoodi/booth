use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use net::natpmp::{
    AddressAnswer, Client, EXTERNAL_ADDRESS_LEN, MAP_LEN, MapAnswer, MapRequest, Mapping,
    NatPmpError, ParseError, ResultCode, external_address_request, map_request,
    parse_external_address, parse_map_response,
};
use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;

const REQUEST: MapRequest = MapRequest {
    internal_port: 41000,
    suggested_port: 41000,
    lifetime: 7200,
};
const DELETION: MapRequest = MapRequest {
    internal_port: 41000,
    suggested_port: 0,
    lifetime: 0,
};
const OUTSIDE: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 7);
const EPOCH: [u8; 4] = [0, 0, 0x10, 0];

// RFC 6886 section 3.2.
fn address_answer(result: u16, ip: Ipv4Addr) -> Vec<u8> {
    let mut msg = vec![0, 128];
    msg.extend_from_slice(&result.to_be_bytes());
    msg.extend_from_slice(&EPOCH);
    msg.extend_from_slice(&ip.octets());
    msg
}

// RFC 6886 section 3.3.
fn map_answer(result: u16, internal: u16, external: u16, lifetime: u32) -> Vec<u8> {
    let mut msg = vec![0, 129];
    msg.extend_from_slice(&result.to_be_bytes());
    msg.extend_from_slice(&EPOCH);
    msg.extend_from_slice(&internal.to_be_bytes());
    msg.extend_from_slice(&external.to_be_bytes());
    msg.extend_from_slice(&lifetime.to_be_bytes());
    msg
}

#[test]
fn requests_match_the_rfc_layout() {
    assert_eq!(external_address_request(), [0, 0]);
    #[rustfmt::skip]
    let want = [
        // Version 0, opcode 1 (UDP), reserved.
        0, 1, 0, 0,
        // Internal port 41000, suggested external port 41000.
        0xa0, 0x28, 0xa0, 0x28,
        // Requested lifetime, 7200 s.
        0x00, 0x00, 0x1c, 0x20,
    ];
    assert_eq!(map_request(&REQUEST), want);
    // A deletion sends port and lifetime 0 (section 3.4).
    assert_eq!(
        map_request(&DELETION),
        [0, 1, 0, 0, 0xa0, 0x28, 0, 0, 0, 0, 0, 0]
    );
}

#[test]
fn successful_answers() {
    assert_eq!(
        parse_external_address(&address_answer(0, OUTSIDE)),
        Ok(AddressAnswer::Address(OUTSIDE))
    );
    // Behind a second router this is a private address, and that is exactly
    // what the second router check wants to see.
    let private = Ipv4Addr::new(192, 168, 1, 2);
    assert_eq!(
        parse_external_address(&address_answer(0, private)),
        Ok(AddressAnswer::Address(private))
    );
    assert_eq!(
        parse_map_response(&map_answer(0, 41000, 50123, 3600), &REQUEST),
        Ok(MapAnswer::Mapped {
            port: 50123,
            lifetime: 3600,
        })
    );
    assert_eq!(
        parse_map_response(&map_answer(0, 41000, 0, 0), &DELETION),
        Ok(MapAnswer::Deleted)
    );
}

#[test]
fn every_result_code_by_name() {
    let names = [
        "success",
        "unsupported version",
        "not authorized or refused",
        "network failure",
        "out of resources",
        "unsupported opcode",
    ];
    for (code, name) in names.iter().enumerate() {
        assert_eq!(ResultCode(code as u16).name(), *name);
    }
    assert_eq!(ResultCode(6).name(), "unknown result");
    assert_eq!(
        ResultCode::NOT_AUTHORIZED.to_string(),
        "not authorized or refused (2)"
    );

    for code in (1..=5).chain([6, 256, 0xffff]) {
        let refused = ResultCode(code);
        let address = address_answer(code, Ipv4Addr::UNSPECIFIED);
        assert_eq!(
            parse_external_address(&address),
            Ok(AddressAnswer::Refused(refused))
        );
        // Section 3.5: an error may be just the first 8 bytes.
        assert_eq!(
            parse_external_address(&address[..8]),
            Ok(AddressAnswer::Refused(refused))
        );
        let map = map_answer(code, 41000, 0, 0);
        assert_eq!(
            parse_map_response(&map, &REQUEST),
            Ok(MapAnswer::Refused(refused))
        );
        assert_eq!(
            parse_map_response(&map[..8], &REQUEST),
            Ok(MapAnswer::Refused(refused))
        );
    }
}

#[test]
fn pcp_only_router_answers_unsupported_version() {
    // RFC 6887 section 9: a PCP server that does not do NAT-PMP answers in
    // its own layout, version 2, with UNSUPP_VERSION in the fourth byte.
    let mut pcp = vec![2, 0x80, 0, 1];
    pcp.resize(24, 0);
    assert_eq!(
        parse_external_address(&pcp),
        Ok(AddressAnswer::UnsupportedVersion(2))
    );
    pcp[1] = 0x81;
    assert_eq!(
        parse_map_response(&pcp, &REQUEST),
        Ok(MapAnswer::UnsupportedVersion(2))
    );
    pcp[3] = 0;
    assert_eq!(
        parse_map_response(&pcp, &REQUEST),
        Err(ParseError::Version(2))
    );
}

#[test]
fn answers_not_ours() {
    // The request itself, reflected back.
    assert_eq!(
        parse_external_address(&[0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4]),
        Err(ParseError::NotAnswer)
    );
    assert_eq!(
        parse_map_response(&map_request(&REQUEST), &REQUEST),
        Err(ParseError::NotAnswer)
    );
    // A late answer to the other kind of request.
    assert_eq!(
        parse_external_address(&map_answer(0, 41000, 41000, 7200)),
        Err(ParseError::Opcode(1))
    );
    assert_eq!(
        parse_map_response(&address_answer(0, OUTSIDE), &REQUEST),
        Err(ParseError::Opcode(0))
    );
    // TCP is opcode 2, which we never ask for.
    let mut tcp = map_answer(0, 41000, 41000, 7200);
    tcp[1] = 130;
    assert_eq!(
        parse_map_response(&tcp, &REQUEST),
        Err(ParseError::Opcode(2))
    );
    let mut v1 = address_answer(0, OUTSIDE);
    v1[0] = 1;
    assert_eq!(parse_external_address(&v1), Err(ParseError::Version(1)));
    assert_eq!(
        parse_map_response(&map_answer(0, 41001, 41000, 7200), &REQUEST),
        Err(ParseError::InternalPort(41001))
    );
}

#[test]
fn short_packets_and_unusable_answers() {
    let address = address_answer(0, OUTSIDE);
    for len in 0..EXTERNAL_ADDRESS_LEN {
        assert_eq!(
            parse_external_address(&address[..len]),
            Err(ParseError::Short(len)),
            "{len}"
        );
    }
    let map = map_answer(0, 41000, 41000, 7200);
    for len in 0..MAP_LEN {
        assert_eq!(
            parse_map_response(&map[..len], &REQUEST),
            Err(ParseError::Short(len)),
            "{len}"
        );
    }
    let mut long = map.clone();
    long.resize(1104, 0);
    assert_eq!(
        parse_map_response(&long, &REQUEST),
        Err(ParseError::Long(1104))
    );
    // Trailing bytes the RFC does not mention carry nothing we read.
    let mut padded = map.clone();
    padded.extend_from_slice(&[0; 4]);
    assert!(matches!(
        parse_map_response(&padded, &REQUEST),
        Ok(MapAnswer::Mapped { .. })
    ));

    assert_eq!(
        parse_external_address(&address_answer(0, Ipv4Addr::UNSPECIFIED)),
        Err(ParseError::ZeroAddress)
    );
    assert_eq!(
        parse_map_response(&map_answer(0, 41000, 0, 7200), &REQUEST),
        Err(ParseError::ZeroPort)
    );
    assert_eq!(
        parse_map_response(&map_answer(0, 41000, 41000, 0), &REQUEST),
        Err(ParseError::ZeroLifetime)
    );
    // The late answer to an earlier map request must not pass for the
    // deletion's.
    assert_eq!(
        parse_map_response(&map, &DELETION),
        Err(ParseError::NotDeleted(7200))
    );
}

// A router on loopback. Every request goes to `reply`, which sends whatever
// it likes from the router's socket to the client; once `requests` have come
// in, the thread hands back what it saw.
fn fake_router(
    requests: usize,
    mut reply: impl FnMut(&[u8], &UdpSocket, SocketAddr) + Send + 'static,
) -> (SocketAddr, JoinHandle<Vec<Vec<u8>>>) {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let addr = socket.local_addr().unwrap();
    let thread = thread::spawn(move || {
        let mut seen = Vec::new();
        let mut buf = [0u8; 2048];
        while seen.len() < requests {
            let Ok((len, from)) = socket.recv_from(&mut buf) else {
                break;
            };
            let request = buf[..len].to_vec();
            reply(&request, &socket, from);
            seen.push(request);
        }
        seen
    });
    (addr, thread)
}

// Answers like a working NAT-PMP router that hands out port 50000.
fn working(request: &[u8], socket: &UdpSocket, client: SocketAddr) {
    let answer = match request {
        [0, 0] => address_answer(0, OUTSIDE),
        [0, 1, _, _, a, b, _, _, c, d, e, f] => {
            let lifetime = u32::from_be_bytes([*c, *d, *e, *f]);
            let port = if lifetime == 0 { 0 } else { 50000 };
            map_answer(0, u16::from_be_bytes([*a, *b]), port, lifetime.min(3600))
        }
        _ => return,
    };
    socket.send_to(&answer, client).unwrap();
}

fn client(router: SocketAddr) -> Client {
    Client::new(router, Ipv4Addr::LOCALHOST, 41000).unwrap()
}

fn quiet(_: std::fmt::Arguments<'_>) {}

const TRIES: [Duration; 2] = [Duration::from_millis(250), Duration::from_millis(500)];

#[test]
fn map_renew_delete() {
    let (router, thread) = fake_router(5, working);
    let mut natpmp = client(router);
    let want = Mapping {
        external: SocketAddrV4::new(OUTSIDE, 50000),
        lifetime: 3600,
    };
    assert_eq!(natpmp.map(41000, 7200, &TRIES, &mut quiet), Ok(want));
    assert_eq!(natpmp.renew(7200, &TRIES, &mut quiet), Ok(want));
    assert_eq!(natpmp.delete(&TRIES, &mut quiet), Ok(()));

    let seen = thread.join().unwrap();
    let want_requests: [&[u8]; 5] = [
        &[0, 0],
        &[0, 1, 0, 0, 0xa0, 0x28, 0xa0, 0x28, 0, 0, 0x1c, 0x20],
        &[0, 0],
        // The renewal suggests the port it was given.
        &[0, 1, 0, 0, 0xa0, 0x28, 0xc3, 0x50, 0, 0, 0x1c, 0x20],
        &[0, 1, 0, 0, 0xa0, 0x28, 0, 0, 0, 0, 0, 0],
    ];
    assert_eq!(seen, want_requests);
}

#[test]
fn external_address_alone() {
    let (router, thread) = fake_router(1, working);
    assert_eq!(
        client(router).external_address(&TRIES, &mut quiet),
        Ok(OUTSIDE)
    );
    thread.join().unwrap();
}

#[test]
fn refusal_stops_before_the_map_request() {
    let (router, thread) = fake_router(1, |_, socket, client| {
        socket
            .send_to(&address_answer(2, Ipv4Addr::UNSPECIFIED)[..8], client)
            .unwrap();
    });
    let err = client(router)
        .map(41000, 7200, &TRIES, &mut quiet)
        .unwrap_err();
    assert_eq!(err, NatPmpError::Refused(ResultCode::NOT_AUTHORIZED));
    assert_eq!(
        err.to_string(),
        "the router refused the nat-pmp request: not authorized or refused (2)"
    );
    assert_eq!(thread.join().unwrap().len(), 1);
}

#[test]
fn only_the_router_itself_is_listened_to() {
    let stranger = UdpSocket::bind("127.0.0.1:0").unwrap();
    let stranger_addr = stranger.local_addr().unwrap();
    let (router, thread) = fake_router(1, move |request, socket, client| {
        stranger
            .send_to(&address_answer(0, Ipv4Addr::new(198, 51, 100, 1)), client)
            .unwrap();
        socket
            .send_to(&map_answer(0, 41000, 41000, 7200), client)
            .unwrap();
        working(request, socket, client);
    });
    let mut notes = Vec::new();
    let got = client(router).external_address(&TRIES, &mut |line| notes.push(line.to_string()));
    assert_eq!(got, Ok(OUTSIDE));
    thread.join().unwrap();
    let ignored: Vec<&String> = notes.iter().filter(|n| n.starts_with("ignored")).collect();
    assert_eq!(
        ignored,
        [
            &format!("ignored 12 bytes from {stranger_addr}: not the router"),
            &format!("ignored 16 bytes from {router}: opcode 1 does not answer ours"),
        ]
    );
}

#[test]
fn silence_and_a_closed_port() {
    let (router, thread) = fake_router(2, |_, _, _| {});
    let tries = [Duration::from_millis(40), Duration::from_millis(80)];
    let got = client(router).external_address(&tries, &mut quiet);
    let Err(NatPmpError::NoAnswer { tries, waited }) = got else {
        panic!("expected no answer, got {got:?}");
    };
    assert_eq!(tries, 2);
    assert!(waited >= Duration::from_millis(120), "{waited:?}");
    assert_eq!(thread.join().unwrap().len(), 2);

    let gone = UdpSocket::bind("127.0.0.1:0").unwrap();
    let router = gone.local_addr().unwrap();
    drop(gone);
    let slow = [Duration::from_secs(2), Duration::from_secs(2)];
    let started = Instant::now();
    assert_eq!(
        client(router).map(41000, 7200, &slow, &mut quiet),
        Err(NatPmpError::PortClosed)
    );
    assert!(started.elapsed() < Duration::from_secs(1));
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 4096,
        failure_persistence: Some(Box::new(FileFailurePersistence::WithSource("regressions"))),
        ..ProptestConfig::default()
    })]

    #[test]
    fn random_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..1200)) {
        let _ = parse_external_address(&bytes);
        let _ = parse_map_response(&bytes, &REQUEST);
        let _ = parse_map_response(&bytes, &DELETION);
    }

    // Keeps the first two bytes plausible to reach past the header check.
    #[test]
    fn random_answers_never_give_an_unusable_one(
        version in prop_oneof![Just(0u8), Just(2u8), any::<u8>()],
        op in prop_oneof![Just(128u8), Just(129u8), any::<u8>()],
        body in proptest::collection::vec(any::<u8>(), 0..20),
    ) {
        let mut msg = vec![version, op];
        msg.extend_from_slice(&body);
        if let Ok(AddressAnswer::Address(ip)) = parse_external_address(&msg) {
            prop_assert!(!ip.is_unspecified());
        }
        if let Ok(MapAnswer::Mapped { port, lifetime }) = parse_map_response(&msg, &REQUEST) {
            prop_assert!(port != 0 && lifetime != 0);
        }
        let _ = parse_map_response(&msg, &DELETION);
    }
}
