use std::net::SocketAddr;

use net::stun::{
    Family, MAGIC_COOKIE, Mapping, StunError, binding_request, classify, is_stun,
    parse_binding_response, resolve,
};
use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;

// RFC 5769 section 2.2, "Sample IPv4 Response", copied from the RFC text.
// The MESSAGE-INTEGRITY and FINGERPRINT values were checked against the RFC's
// password and CRC-32 when this was written, so the bytes are exact.
const RFC5769_IPV4_RESPONSE: [u8; 80] = [
    0x01, 0x01, 0x00, 0x3c, 0x21, 0x12, 0xa4, 0x42, 0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86,
    0xfa, 0x87, 0xdf, 0xae, 0x80, 0x22, 0x00, 0x0b, 0x74, 0x65, 0x73, 0x74, 0x20, 0x76, 0x65, 0x63,
    0x74, 0x6f, 0x72, 0x20, 0x00, 0x20, 0x00, 0x08, 0x00, 0x01, 0xa1, 0x47, 0xe1, 0x12, 0xa6, 0x43,
    0x00, 0x08, 0x00, 0x14, 0x2b, 0x91, 0xf5, 0x99, 0xfd, 0x9e, 0x90, 0xc3, 0x8c, 0x74, 0x89, 0xf9,
    0x2a, 0xf9, 0xba, 0x53, 0xf0, 0x6b, 0xe7, 0xd7, 0x80, 0x28, 0x00, 0x04, 0xc0, 0x7d, 0x4c, 0x96,
];

// RFC 5769 section 2.3, "Sample IPv6 Response", copied from the RFC text.
const RFC5769_IPV6_RESPONSE: [u8; 92] = [
    0x01, 0x01, 0x00, 0x48, 0x21, 0x12, 0xa4, 0x42, 0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86,
    0xfa, 0x87, 0xdf, 0xae, 0x80, 0x22, 0x00, 0x0b, 0x74, 0x65, 0x73, 0x74, 0x20, 0x76, 0x65, 0x63,
    0x74, 0x6f, 0x72, 0x20, 0x00, 0x20, 0x00, 0x14, 0x00, 0x02, 0xa1, 0x47, 0x01, 0x13, 0xa9, 0xfa,
    0xa5, 0xd3, 0xf1, 0x79, 0xbc, 0x25, 0xf4, 0xb5, 0xbe, 0xd2, 0xb9, 0xd9, 0x00, 0x08, 0x00, 0x14,
    0xa3, 0x82, 0x95, 0x4e, 0x4b, 0xe6, 0x7b, 0xf1, 0x17, 0x84, 0xc9, 0x7c, 0x82, 0x92, 0xc2, 0x75,
    0xbf, 0xe3, 0xed, 0x41, 0x80, 0x28, 0x00, 0x04, 0xc8, 0xfb, 0x0b, 0x4c,
];

const RFC5769_TXID: [u8; 12] = [
    0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae,
];

fn addr(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

// A success response header with the given attributes, length filled in.
fn response(attrs: &[u8]) -> Vec<u8> {
    let mut msg = vec![0x01, 0x01];
    msg.extend_from_slice(&(attrs.len() as u16).to_be_bytes());
    msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    msg.extend_from_slice(&RFC5769_TXID);
    msg.extend_from_slice(attrs);
    msg
}

#[test]
fn rfc5769_ipv4_response() {
    assert!(is_stun(&RFC5769_IPV4_RESPONSE));
    let (txid, mapped) = parse_binding_response(&RFC5769_IPV4_RESPONSE).unwrap();
    assert_eq!(txid, RFC5769_TXID);
    assert_eq!(mapped, addr("192.0.2.1:32853"));
}

#[test]
fn rfc5769_ipv6_response() {
    assert!(is_stun(&RFC5769_IPV6_RESPONSE));
    let (txid, mapped) = parse_binding_response(&RFC5769_IPV6_RESPONSE).unwrap();
    assert_eq!(txid, RFC5769_TXID);
    assert_eq!(mapped, addr("[2001:db8:1234:5678:11:2233:4455:6677]:32853"));
}

#[test]
fn request_is_a_bare_20_byte_header() {
    let req = binding_request(&RFC5769_TXID);
    assert_eq!(req.len(), 20);
    assert_eq!(req[..8], [0x00, 0x01, 0x00, 0x00, 0x21, 0x12, 0xa4, 0x42]);
    assert_eq!(req[8..], RFC5769_TXID);
    assert!(is_stun(&req));
    assert_eq!(
        parse_binding_response(&req),
        Err(StunError::NotBindingResponse(0x0001))
    );
}

#[test]
fn our_own_packets_are_not_stun() {
    let mut data = [0u8; 64];
    data[0] = 0x11;
    assert!(!is_stun(&data));
    // Even with the cookie where a session index would be, the length is wrong.
    data[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    assert!(!is_stun(&data));
    assert!(!is_stun(&[]));
    assert!(!is_stun(&RFC5769_IPV4_RESPONSE[..79]));
}

#[test]
fn mapped_address_is_the_fallback_and_xor_wins() {
    let plain = [0x00, 0x01, 0x00, 0x08, 0x00, 0x01, 0x0f, 0xa0, 10, 0, 0, 1];
    let (_, got) = parse_binding_response(&response(&plain)).unwrap();
    assert_eq!(got, addr("10.0.0.1:4000"));

    // Plain first, XOR second: the XOR form still wins.
    let mut both = plain.to_vec();
    both.extend_from_slice(&RFC5769_IPV4_RESPONSE[36..48]);
    let (_, got) = parse_binding_response(&response(&both)).unwrap();
    assert_eq!(got, addr("192.0.2.1:32853"));
}

#[test]
fn broken_responses_are_errors() {
    assert_eq!(
        parse_binding_response(&response(&[])),
        Err(StunError::NoAddress)
    );
    // Attribute claims 12 bytes of value, the message holds 8.
    let overrun = [
        0x00, 0x20, 0x00, 0x0c, 0x00, 0x01, 0xa1, 0x47, 0xe1, 0x12, 0xa6, 0x43,
    ];
    assert_eq!(
        parse_binding_response(&response(&overrun)),
        Err(StunError::Truncated)
    );
    // IPv6 family with an IPv4-sized value.
    let wrong_family = [
        0x00, 0x20, 0x00, 0x08, 0x00, 0x02, 0xa1, 0x47, 0xe1, 0x12, 0xa6, 0x43,
    ];
    assert_eq!(
        parse_binding_response(&response(&wrong_family)),
        Err(StunError::BadAddress)
    );

    let mut refused = response(&[0x00, 0x09, 0x00, 0x04, 0x00, 0x00, 0x04, 0x20]);
    refused[1] = 0x11;
    assert_eq!(
        parse_binding_response(&refused),
        Err(StunError::Refused(432))
    );
}

#[test]
fn classify_mappings() {
    let cloudflare = addr("162.159.207.0:3478");
    let google = addr("74.125.250.129:19302");
    let outside = addr("203.0.113.7:41000");
    let v4 = |answers: &[(SocketAddr, SocketAddr)]| classify(answers, Family::Ipv4);

    assert_eq!(v4(&[]), Mapping::Unknown);
    assert_eq!(v4(&[(cloudflare, outside)]), Mapping::Unknown);
    assert_eq!(
        v4(&[(cloudflare, outside), (cloudflare, outside)]),
        Mapping::Unknown
    );
    assert_eq!(
        v4(&[(cloudflare, outside), (google, outside)]),
        Mapping::Easy
    );
    assert_eq!(
        v4(&[(cloudflare, outside), (google, addr("203.0.113.7:52011"))]),
        Mapping::Hard
    );
    assert_eq!(
        v4(&[(cloudflare, outside), (google, addr("203.0.113.8:41000"))]),
        Mapping::Hard
    );
    assert_eq!(
        v4(&[
            (cloudflare, outside),
            (google, outside),
            (addr("192.0.2.50:3478"), addr("203.0.113.7:41001")),
        ]),
        Mapping::Hard
    );
    // Two ports on one server IP agreeing proves nothing about the mapping.
    assert_eq!(
        v4(&[(cloudflare, outside), (addr("162.159.207.0:3479"), outside)]),
        Mapping::Unknown
    );
    // No NAT on IPv6: the mapped address is the PC's own, and it is easy.
    let v6 = addr("[2001:db8::10]:41000");
    assert_eq!(
        classify(
            &[
                (addr("[2606:4700::1]:3478"), v6),
                (addr("[2001:4860::1]:19302"), v6)
            ],
            Family::Ipv6
        ),
        Mapping::Easy
    );
}

// The host asks STUN over both families at once, so the room may collect
// every answer into one list in whatever order they arrive.
#[test]
fn classify_ignores_answer_order() {
    let cloudflare = addr("162.159.207.0:3478");
    let google = addr("74.125.250.129:19302");
    let outside = addr("203.0.113.7:41000");
    let cloudflare_v6 = addr("[2606:4700::1]:3478");
    let google_v6 = addr("[2001:4860::1]:19302");
    let own_v6 = addr("[2001:db8::10]:41000");

    let v4_first = [
        (cloudflare, outside),
        (google, outside),
        (cloudflare_v6, own_v6),
    ];
    let v6_first = [
        (cloudflare_v6, own_v6),
        (cloudflare, outside),
        (google, outside),
    ];
    for answers in [&v4_first, &v6_first] {
        assert_eq!(classify(answers, Family::Ipv4), Mapping::Easy);
        assert_eq!(classify(answers, Family::Ipv6), Mapping::Unknown);
    }

    // A pair whose server and mapped address disagree on family is not an
    // answer for either.
    let odd = [
        (cloudflare, own_v6),
        (google, outside),
        (cloudflare_v6, outside),
        (google_v6, own_v6),
    ];
    assert_eq!(classify(&odd, Family::Ipv4), Mapping::Unknown);
    assert_eq!(classify(&odd, Family::Ipv6), Mapping::Unknown);
}

#[test]
fn resolve_takes_literals_and_bad_input() {
    assert_eq!(resolve("127.0.0.1:3478"), vec![addr("127.0.0.1:3478")]);
    assert_eq!(resolve(" 127.0.0.1 "), vec![addr("127.0.0.1:3478")]);
    assert_eq!(resolve("[::1]:19302"), vec![addr("[::1]:19302")]);
    assert_eq!(resolve("[::1]"), vec![addr("[::1]:3478")]);
    for bad in [
        "",
        "   ",
        ":",
        "host:port",
        "127.0.0.1:99999",
        "a\0b:3478",
        "[::1",
    ] {
        assert!(resolve(bad).is_empty(), "{bad:?}");
    }
    let local = resolve("localhost:3478");
    assert!(!local.is_empty());
    for (i, a) in local.iter().enumerate() {
        assert!(!local[i + 1..].contains(a), "duplicate {a} in {local:?}");
    }
}

fn attribute_type() -> impl Strategy<Value = u16> {
    prop_oneof![
        Just(0x0001u16),
        Just(0x0009u16),
        Just(0x0020u16),
        any::<u16>()
    ]
}

// Pure noise almost never gets past the header check, so the second and third
// cases put random bodies and well-framed random attributes behind a valid
// header to reach the attribute walk and the address decoding.
proptest! {
    // An integration test has no lib.rs beside it for proptest's default
    // regression folder, so failures are saved next to this file instead.
    #![proptest_config(ProptestConfig {
        cases: 4096,
        failure_persistence: Some(Box::new(FileFailurePersistence::WithSource("regressions"))),
        ..ProptestConfig::default()
    })]

    #[test]
    fn random_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..600)) {
        let _ = is_stun(&bytes);
        let _ = parse_binding_response(&bytes);
    }

    #[test]
    fn random_bodies_never_panic(
        body in proptest::collection::vec(any::<u8>(), 0..150),
        ty in prop_oneof![Just(0x0101u16), Just(0x0111u16), any::<u16>()],
    ) {
        let mut body = body;
        body.truncate(body.len() / 4 * 4);
        let mut msg = response(&body);
        msg[0..2].copy_from_slice(&ty.to_be_bytes());
        let _ = is_stun(&msg);
        let _ = parse_binding_response(&msg);
    }

    #[test]
    fn random_attributes_never_panic(
        attrs in proptest::collection::vec(
            (attribute_type(), proptest::collection::vec(any::<u8>(), 0..24)),
            0..6,
        ),
        error in any::<bool>(),
    ) {
        let mut body = Vec::new();
        for (ty, value) in &attrs {
            body.extend_from_slice(&ty.to_be_bytes());
            body.extend_from_slice(&(value.len() as u16).to_be_bytes());
            body.extend_from_slice(value);
            body.resize(body.len().next_multiple_of(4), 0);
        }
        let mut msg = response(&body);
        if error {
            msg[1] = 0x11;
        }
        prop_assert!(is_stun(&msg));
        let _ = parse_binding_response(&msg);
    }
}
