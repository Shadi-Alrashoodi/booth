use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use net::pcp::{
    Answer, Client, MAP_LEN, MAX_MESSAGE, MapRequest, Mapping, ParseError, PcpError, ResultCode,
    map_request, parse_map_response,
};
use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;

const NONCE: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
const REQUEST: MapRequest = MapRequest {
    nonce: NONCE,
    client: Ipv4Addr::new(192, 168, 100, 38),
    internal_port: 41000,
    suggested: SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 41000),
    lifetime: 7200,
};
const OUTSIDE: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 7), 41000);
const NOWHERE: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0);

// A router's answer to `request` (the bytes as sent), laid out as in RFC 6887
// sections 7.2 and 11.1, with the nonce, protocol and internal port copied.
fn answer(request: &[u8], result: u8, lifetime: u32, external: SocketAddrV4) -> Vec<u8> {
    let mut msg = vec![2, 0x81, 0, result];
    msg.extend_from_slice(&lifetime.to_be_bytes());
    msg.extend_from_slice(&1234u32.to_be_bytes());
    msg.extend_from_slice(&[0; 12]);
    msg.extend_from_slice(&request[24..42]);
    msg.extend_from_slice(&external.port().to_be_bytes());
    msg.extend_from_slice(&external.ip().to_ipv6_mapped().octets());
    msg
}

fn success() -> Vec<u8> {
    answer(&map_request(&REQUEST), 0, 7200, OUTSIDE)
}

fn deletion() -> MapRequest {
    MapRequest {
        lifetime: 0,
        ..REQUEST
    }
}

#[test]
fn map_request_matches_the_rfc_layout() {
    #[rustfmt::skip]
    let want: [u8; MAP_LEN] = [
        // Version 2, R clear and opcode MAP, reserved.
        0x02, 0x01, 0x00, 0x00,
        // Requested lifetime, 7200 s.
        0x00, 0x00, 0x1c, 0x20,
        // Client address, ::ffff:192.168.100.38.
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 192, 168, 100, 38,
        // Mapping nonce.
        1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12,
        // Protocol UDP, reserved.
        17, 0, 0, 0,
        // Internal port 41000, suggested external port 41000.
        0xa0, 0x28, 0xa0, 0x28,
        // Suggested external address, ::ffff:0.0.0.0 for "any".
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0, 0, 0, 0,
    ];
    assert_eq!(map_request(&REQUEST), want);

    let renewal = MapRequest {
        suggested: OUTSIDE,
        lifetime: 0,
        ..REQUEST
    };
    let got = map_request(&renewal);
    assert_eq!(got[4..8], [0, 0, 0, 0]);
    assert_eq!(got[42..44], [0xa0, 0x28]);
    assert_eq!(got[44..60], OUTSIDE.ip().to_ipv6_mapped().octets());
}

#[test]
fn successful_answer() {
    assert_eq!(
        parse_map_response(&success(), &REQUEST),
        Ok(Answer::Mapped(Mapping {
            external: OUTSIDE,
            lifetime: 7200,
        }))
    );
    // Another port and a shorter lifetime than asked for are the router's
    // call, not an error.
    let other = SocketAddrV4::new(*OUTSIDE.ip(), 50123);
    let msg = answer(&map_request(&REQUEST), 0, 600, other);
    assert_eq!(
        parse_map_response(&msg, &REQUEST),
        Ok(Answer::Mapped(Mapping {
            external: other,
            lifetime: 600,
        }))
    );
}

#[test]
fn every_result_code_by_name() {
    let names = [
        "SUCCESS",
        "UNSUPP_VERSION",
        "NOT_AUTHORIZED",
        "MALFORMED_REQUEST",
        "UNSUPP_OPCODE",
        "UNSUPP_OPTION",
        "MALFORMED_OPTION",
        "NETWORK_FAILURE",
        "NO_RESOURCES",
        "UNSUPP_PROTOCOL",
        "USER_EX_QUOTA",
        "CANNOT_PROVIDE_EXTERNAL",
        "ADDRESS_MISMATCH",
        "EXCESSIVE_REMOTE_PEERS",
    ];
    for (code, name) in names.iter().enumerate() {
        assert_eq!(ResultCode(code as u8).name(), *name);
    }
    assert_eq!(ResultCode(14).name(), "unknown result");
    assert_eq!(ResultCode::NOT_AUTHORIZED.to_string(), "NOT_AUTHORIZED (2)");

    let request = map_request(&REQUEST);
    for code in (2..=13).chain([14, 200, 255]) {
        let refused = Ok(Answer::Refused {
            code: ResultCode(code),
            lifetime: 30,
        });
        // Error answers carry the request's fields back, or just the header
        // when the router could not read them.
        let full = answer(&request, code, 30, NOWHERE);
        assert_eq!(parse_map_response(&full, &REQUEST), refused, "{code}");
        assert_eq!(parse_map_response(&full[..24], &REQUEST), refused, "{code}");
    }
}

#[test]
fn unsupported_version_from_a_nat_pmp_router() {
    // RFC 6886 section 3.5: version 0, opcode 128 + 1, 16-bit result 1, epoch.
    let nat_pmp = [0, 0x81, 0, 1, 0, 0, 0x10, 0x00];
    assert_eq!(
        parse_map_response(&nat_pmp, &REQUEST),
        Ok(Answer::UnsupportedVersion(0))
    );
    // An early PCP draft server says so in PCP's own layout.
    let mut draft = answer(&map_request(&REQUEST), 1, 0, OUTSIDE);
    draft[0] = 1;
    assert_eq!(
        parse_map_response(&draft[..24], &REQUEST),
        Ok(Answer::UnsupportedVersion(1))
    );
    // Version 0 with any other result is not an answer to us.
    let odd = [0, 0x81, 0, 0, 0, 0, 0x10, 0x00];
    assert_eq!(
        parse_map_response(&odd, &REQUEST),
        Err(ParseError::Version(0))
    );
}

#[test]
fn answers_not_ours() {
    let good = success();
    let with = |at: usize, value: u8| {
        let mut msg = good.clone();
        msg[at] = value;
        parse_map_response(&msg, &REQUEST)
    };
    assert_eq!(with(0, 3), Err(ParseError::Version(3)));
    // Our own request, reflected back.
    assert_eq!(with(1, 0x01), Err(ParseError::NotAnswer));
    assert_eq!(with(1, 0x82), Err(ParseError::Opcode(2)));
    assert_eq!(with(1, 0x80), Err(ParseError::Opcode(0)));
    assert_eq!(with(24, 0xff), Err(ParseError::Nonce));
    assert_eq!(with(35, 0), Err(ParseError::Nonce));
    assert_eq!(with(36, 6), Err(ParseError::Protocol(6)));
    assert_eq!(with(41, 0x29), Err(ParseError::InternalPort(41001)));
    assert_eq!(
        parse_map_response(&map_request(&REQUEST), &REQUEST),
        Err(ParseError::NotAnswer)
    );
    // A refusal for someone else's nonce is not ours either.
    let mut refused = answer(&map_request(&REQUEST), 2, 30, OUTSIDE);
    refused[30] ^= 1;
    assert_eq!(
        parse_map_response(&refused, &REQUEST),
        Err(ParseError::Nonce)
    );
}

#[test]
fn short_long_and_ragged_packets() {
    let good = success();
    for len in 0..4 {
        assert_eq!(
            parse_map_response(&good[..len], &REQUEST),
            Err(ParseError::Short(len))
        );
    }
    for len in 4..MAP_LEN {
        let got = parse_map_response(&good[..len], &REQUEST);
        let want = if len % 4 != 0 {
            ParseError::Unaligned(len)
        } else {
            ParseError::Short(len)
        };
        assert_eq!(got, Err(want), "{len}");
    }
    let mut long = good.clone();
    long.resize(MAX_MESSAGE + 4, 0);
    assert_eq!(
        parse_map_response(&long, &REQUEST),
        Err(ParseError::Long(MAX_MESSAGE + 4))
    );
}

#[test]
fn unusable_mappings_are_refused() {
    let request = map_request(&REQUEST);
    let zero_ip = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 41000);
    assert_eq!(
        parse_map_response(&answer(&request, 0, 7200, zero_ip), &REQUEST),
        Err(ParseError::ZeroAddress)
    );
    let zero_port = SocketAddrV4::new(*OUTSIDE.ip(), 0);
    assert_eq!(
        parse_map_response(&answer(&request, 0, 7200, zero_port), &REQUEST),
        Err(ParseError::ZeroPort)
    );
    assert_eq!(
        parse_map_response(&answer(&request, 0, 0, OUTSIDE), &REQUEST),
        Err(ParseError::ZeroLifetime)
    );
    let ip: Ipv6Addr = "2001:db8::7".parse().unwrap();
    let mut v6 = success();
    v6[44..60].copy_from_slice(&ip.octets());
    assert_eq!(
        parse_map_response(&v6, &REQUEST),
        Err(ParseError::NotIpv4(ip))
    );
}

#[test]
fn deletion_answers() {
    let request = map_request(&deletion());
    let deleted = answer(&request, 0, 0, OUTSIDE);
    assert_eq!(
        parse_map_response(&deleted, &deletion()),
        Ok(Answer::Deleted)
    );
    // The late answer to an earlier map request must not pass for it.
    assert_eq!(
        parse_map_response(&success(), &deletion()),
        Err(ParseError::NotDeleted(7200))
    );
    let refused = answer(&request, 2, 0, OUTSIDE);
    assert_eq!(
        parse_map_response(&refused, &deletion()),
        Ok(Answer::Refused {
            code: ResultCode::NOT_AUTHORIZED,
            lifetime: 0,
        })
    );
}

#[test]
fn options_are_walked_not_read() {
    let mut msg = success();
    // THIRD_PARTY (code 1) with a 16-byte address, then an unknown optional
    // one with 3 bytes of data and its padding.
    msg.extend_from_slice(&[1, 0, 0, 16]);
    msg.extend_from_slice(&[0; 16]);
    msg.extend_from_slice(&[200, 0, 0, 3, 7, 7, 7, 0]);
    assert!(matches!(
        parse_map_response(&msg, &REQUEST),
        Ok(Answer::Mapped(_))
    ));

    let mut overrun = success();
    overrun.extend_from_slice(&[1, 0, 0, 16, 0, 0, 0, 0]);
    assert_eq!(
        parse_map_response(&overrun, &REQUEST),
        Err(ParseError::BadOption)
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

fn client(router: SocketAddr) -> Client {
    Client::new(router, Ipv4Addr::LOCALHOST, 41000, NONCE).unwrap()
}

fn quiet(_: std::fmt::Arguments<'_>) {}

const TRIES: [Duration; 2] = [Duration::from_millis(250), Duration::from_millis(500)];

#[test]
fn map_renew_delete_with_one_nonce() {
    let granted = SocketAddrV4::new(*OUTSIDE.ip(), 50000);
    let (router, thread) = fake_router(3, move |request, socket, client| {
        let asked = u32::from_be_bytes(request[4..8].try_into().unwrap());
        let lifetime = if asked == 0 { 0 } else { 3600 };
        socket
            .send_to(&answer(request, 0, lifetime, granted), client)
            .unwrap();
    });
    let mut pcp = client(router);
    let mut notes = Vec::new();
    let mut note = |line: std::fmt::Arguments<'_>| notes.push(line.to_string());

    let want = Mapping {
        external: granted,
        lifetime: 3600,
    };
    assert_eq!(pcp.map(41000, 7200, &TRIES, &mut note), Ok(want));
    assert_eq!(pcp.renew(7200, &TRIES, &mut note), Ok(want));
    assert_eq!(pcp.delete(&TRIES, &mut note), Ok(()));

    let seen = thread.join().unwrap();
    assert_eq!(seen.len(), 3);
    let loopback = Ipv4Addr::LOCALHOST.to_ipv6_mapped().octets();
    for request in &seen {
        assert_eq!(request.len(), MAP_LEN);
        assert_eq!(request[8..24], loopback);
        assert_eq!(request[24..36], NONCE);
    }
    // The renewal suggests what was granted; the deletion asks for 0 s.
    assert_eq!(seen[0][42..44], 41000u16.to_be_bytes());
    assert_eq!(
        seen[0][44..60],
        Ipv4Addr::UNSPECIFIED.to_ipv6_mapped().octets()
    );
    assert_eq!(seen[1][42..44], 50000u16.to_be_bytes());
    assert_eq!(seen[1][44..60], OUTSIDE.ip().to_ipv6_mapped().octets());
    assert_eq!(seen[2][4..8], [0, 0, 0, 0]);

    assert!(notes.iter().any(|n| n.starts_with("asking 127.0.0.1:")));
    assert!(
        notes
            .iter()
            .any(|n| n == "request 1 of 2 sent, waiting 250 ms")
    );
    assert!(
        notes
            .iter()
            .any(|n| n.starts_with("answer to request 1 after"))
    );
}

#[test]
fn nat_pmp_router_answers_unsupported_version() {
    let (router, thread) = fake_router(1, |_, socket, client| {
        socket
            .send_to(&[0, 0x81, 0, 1, 0, 0, 0x10, 0x00], client)
            .unwrap();
    });
    let err = client(router).map(41000, 7200, &TRIES, &mut quiet);
    assert_eq!(err, Err(PcpError::UnsupportedVersion(0)));
    assert_eq!(
        err.unwrap_err().to_string(),
        "the router speaks nat-pmp, not pcp"
    );
    thread.join().unwrap();
}

#[test]
fn refusal_is_reported_by_name() {
    let (router, thread) = fake_router(1, |request, socket, client| {
        socket
            .send_to(&answer(request, 2, 60, OUTSIDE), client)
            .unwrap();
    });
    let err = client(router)
        .map(41000, 7200, &TRIES, &mut quiet)
        .unwrap_err();
    assert_eq!(
        err,
        PcpError::Refused {
            code: ResultCode::NOT_AUTHORIZED,
            lifetime: 60,
        }
    );
    assert_eq!(
        err.to_string(),
        "the router refused the pcp mapping: NOT_AUTHORIZED (2), the same for the next 60 s"
    );
    thread.join().unwrap();
}

#[test]
fn only_the_router_itself_is_listened_to() {
    let stranger = UdpSocket::bind("127.0.0.1:0").unwrap();
    let stranger_addr = stranger.local_addr().unwrap();
    let (router, thread) = fake_router(1, move |request, socket, client| {
        let elsewhere = SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 1), 41000);
        stranger
            .send_to(&answer(request, 0, 7200, elsewhere), client)
            .unwrap();
        let mut not_our_nonce = answer(request, 0, 7200, elsewhere);
        not_our_nonce[24] ^= 0xff;
        socket.send_to(&not_our_nonce, client).unwrap();
        socket.send_to(&[0, 0x81], client).unwrap();
        socket
            .send_to(&answer(request, 0, 7200, OUTSIDE), client)
            .unwrap();
    });
    let mut notes = Vec::new();
    let got = client(router).map(41000, 7200, &TRIES, &mut |line| {
        notes.push(line.to_string())
    });
    assert_eq!(
        got,
        Ok(Mapping {
            external: OUTSIDE,
            lifetime: 7200,
        })
    );
    thread.join().unwrap();
    let ignored: Vec<&String> = notes.iter().filter(|n| n.starts_with("ignored")).collect();
    assert_eq!(
        ignored,
        [
            &format!("ignored 60 bytes from {stranger_addr}: not the router"),
            &format!("ignored 60 bytes from {router}: the mapping nonce is not ours"),
            &format!("ignored 2 bytes from {router}: 2 bytes is too short"),
        ]
    );
}

#[test]
fn silence_is_no_answer_after_every_try() {
    let (router, thread) = fake_router(2, |_, _, _| {});
    let tries = [Duration::from_millis(40), Duration::from_millis(80)];
    let started = Instant::now();
    let got = client(router).map(41000, 7200, &tries, &mut quiet);
    let Err(PcpError::NoAnswer { tries, waited }) = got else {
        panic!("expected no answer, got {got:?}");
    };
    assert_eq!(tries, 2);
    assert!(waited >= Duration::from_millis(120), "{waited:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
    let seen = thread.join().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0], seen[1]);
}

#[test]
fn second_try_is_enough() {
    let mut count = 0;
    let (router, thread) = fake_router(2, move |request, socket, client| {
        count += 1;
        if count == 2 {
            socket
                .send_to(&answer(request, 0, 7200, OUTSIDE), client)
                .unwrap();
        }
    });
    let tries = [Duration::from_millis(40), Duration::from_millis(500)];
    assert!(client(router).map(41000, 7200, &tries, &mut quiet).is_ok());
    assert_eq!(thread.join().unwrap().len(), 2);
}

#[test]
fn closed_port_is_reported_without_waiting() {
    let gone = UdpSocket::bind("127.0.0.1:0").unwrap();
    let router = gone.local_addr().unwrap();
    drop(gone);
    let slow = [Duration::from_secs(2), Duration::from_secs(2)];
    let started = Instant::now();
    let got = client(router).map(41000, 7200, &slow, &mut quiet);
    assert_eq!(got, Err(PcpError::PortClosed));
    assert!(started.elapsed() < Duration::from_secs(1));
}

fn version() -> impl Strategy<Value = u8> {
    prop_oneof![Just(2u8), Just(0u8), Just(1u8), any::<u8>()]
}

fn opcode() -> impl Strategy<Value = u8> {
    prop_oneof![Just(0x81u8), Just(0x01u8), any::<u8>()]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 4096,
        failure_persistence: Some(Box::new(FileFailurePersistence::WithSource("regressions"))),
        ..ProptestConfig::default()
    })]

    #[test]
    fn random_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..1200)) {
        let _ = parse_map_response(&bytes, &REQUEST);
        let _ = parse_map_response(&bytes, &deletion());
    }

    // Noise almost never gets past the first bytes, so this keeps a plausible
    // header and, often, our nonce to reach the rest of the parser.
    #[test]
    fn random_answers_never_give_an_unusable_mapping(
        version in version(),
        op in opcode(),
        result in prop_oneof![Just(0u8), any::<u8>()],
        body in proptest::collection::vec(any::<u8>(), 0..160),
        keep_nonce in any::<bool>(),
    ) {
        let mut msg = vec![version, op, 0, result];
        msg.extend_from_slice(&body);
        msg.truncate(msg.len() / 4 * 4);
        if keep_nonce && msg.len() >= MAP_LEN {
            msg[24..36].copy_from_slice(&NONCE);
            msg[36] = 17;
            msg[40..42].copy_from_slice(&41000u16.to_be_bytes());
        }
        if let Ok(Answer::Mapped(mapping)) = parse_map_response(&msg, &REQUEST) {
            prop_assert!(!mapping.external.ip().is_unspecified());
            prop_assert!(mapping.external.port() != 0);
            prop_assert!(mapping.lifetime != 0);
        }
        let _ = parse_map_response(&msg, &deletion());
    }

    #[test]
    fn random_options_never_panic(
        options in proptest::collection::vec(
            (any::<u8>(), any::<u16>(), proptest::collection::vec(any::<u8>(), 0..24)),
            0..5,
        ),
    ) {
        let mut msg = success();
        for (code, len, data) in &options {
            msg.extend_from_slice(&[*code, 0]);
            msg.extend_from_slice(&len.to_be_bytes());
            msg.extend_from_slice(data);
        }
        msg.truncate(msg.len() / 4 * 4);
        let _ = parse_map_response(&msg, &REQUEST);
    }
}
