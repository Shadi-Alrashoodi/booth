// Each decode rule gets its own body, built byte by byte from the format with its own checksum
// and base32, so a rule lost in a refactor fails here by name. The random tests in props.rs
// rarely produce a body that is valid in every way but one.

use std::net::{Ipv6Addr, SocketAddr};

use blake2::{Blake2s256, Digest};
use invite::{Answers, CandidateKind, CodeError, Invite, Mapping, ReplyCode, VERSION, Version};

// An invite body starts with its layout, then the protocol and the Booth version that made it,
// each a big-endian u16. The version is the crate's, since encode writes this build's.
const INVITE_LAYOUT: u8 = 2;
const PROTOCOL: u16 = 1;
const PREAMBLE: usize = 1 + 2 + 6;
const REPLY_LAYOUT: u8 = 1;
const EXPIRES: u32 = 1_800_000_000;

const MULTI_USE: u8 = 1;
const MAPPED: u8 = 1 << 1;
const VERIFIED: u8 = 1 << 2;
const SECOND_ROUTER: u8 = 1 << 3;
const HARD: u8 = 2 << 4;
const HAS_HOSTNAME: u8 = 1 << 6;

const LAN: u8 = 0;
const VPN: u8 = 1;
const IPV6: u8 = 2;
const PUBLIC: u8 = 3;

const REJOIN: u8 = 1;
const HAS_V4: u8 = 1 << 1;
const HAS_V6: u8 = 1 << 2;
const REPLY_EASY: u8 = 1 << 3;

// Bit by bit, unlike the crate's codec, so a shared mistake is unlikely.
fn base32(data: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let bits: Vec<u8> = data
        .iter()
        .flat_map(|byte| (0..8).rev().map(move |i| (byte >> i) & 1))
        .collect();
    bits.chunks(5)
        .map(|chunk| {
            let value = (0..5).fold(0, |acc, i| (acc << 1) | chunk.get(i).copied().unwrap_or(0));
            char::from(ALPHABET[usize::from(value)])
        })
        .collect()
}

fn seal(prefix: &str, domain: &[u8], body: &[u8]) -> String {
    let hash = Blake2s256::new()
        .chain_update(domain)
        .chain_update(body)
        .finalize();
    let mut sealed = body.to_vec();
    sealed.extend_from_slice(&hash[..4]);
    format!("{prefix}{}", base32(&sealed))
}

fn invite_text(body: &[u8]) -> String {
    seal("booth1-", b"booth invite", body)
}

fn reply_text(body: &[u8]) -> String {
    seal("booth1-r-", b"booth reply", body)
}

fn preamble(layout: u8, protocol: u16, version: Version) -> Vec<u8> {
    let mut out = vec![layout];
    for part in [protocol, version.major, version.minor, version.patch] {
        out.extend_from_slice(&part.to_be_bytes());
    }
    out
}

// flags is written as given, so a test can set the host name bit without a name or the reverse.
fn invite_body(flags: u8, candidates: &[Vec<u8>], hostname: Option<&[u8]>) -> Vec<u8> {
    let mut body = preamble(INVITE_LAYOUT, PROTOCOL, VERSION);
    body.push(flags);
    invite_fields(&mut body, candidates, hostname);
    body
}

// Everything after the flags, the same in layout 1.
fn invite_fields(body: &mut Vec<u8>, candidates: &[Vec<u8>], hostname: Option<&[u8]>) {
    body.extend_from_slice(&[7; 32]);
    body.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
    body.extend_from_slice(&[0xab; 16]);
    body.extend_from_slice(&EXPIRES.to_be_bytes());
    body.push(u8::try_from(candidates.len()).unwrap());
    for candidate in candidates {
        body.extend_from_slice(candidate);
    }
    if let Some(name) = hostname {
        body.push(u8::try_from(name.len()).unwrap());
        body.extend_from_slice(name);
    }
}

fn v4(kind: u8, ip: [u8; 4], port: u16) -> Vec<u8> {
    let mut out = vec![0x40 | kind];
    out.extend_from_slice(&ip);
    out.extend_from_slice(&port.to_be_bytes());
    out
}

fn v6(kind: u8, ip: &str, port: u16) -> Vec<u8> {
    let mut out = vec![0x60 | kind];
    out.extend_from_slice(&ip.parse::<Ipv6Addr>().unwrap().octets());
    out.extend_from_slice(&port.to_be_bytes());
    out
}

fn good_candidates() -> Vec<Vec<u8>> {
    vec![
        v4(LAN, [192, 168, 1, 20], 41000),
        v4(VPN, [100, 64, 1, 2], 41000),
        v6(VPN, "fd7a:115c:a1e0::1", 41000),
        v6(IPV6, "2a02:8071::2", 41000),
        v4(PUBLIC, [198, 51, 100, 9], 41000),
    ]
}

fn with_candidate(candidate: Vec<u8>) -> Vec<u8> {
    invite_body(MAPPED, &[candidate], None)
}

fn assert_invite_damaged(body: &[u8], what: &str) {
    assert_eq!(
        Invite::decode(&invite_text(body)),
        Err(CodeError::Damaged),
        "{what}"
    );
}

fn assert_invite_opens(body: &[u8], what: &str) {
    assert!(Invite::decode(&invite_text(body)).is_ok(), "{what}");
}

#[test]
fn hand_built_invite() {
    let flags = MULTI_USE | MAPPED | VERIFIED | SECOND_ROUTER | HARD | HAS_HOSTNAME;
    let body = invite_body(flags, &good_candidates(), Some(b"home.example.net"));
    let invite = Invite::decode(&invite_text(&body)).unwrap();
    assert_eq!(invite.host_key, [7; 32]);
    assert_eq!(invite.invite_id, [1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(invite.secret, [0xab; 16]);
    assert_eq!(invite.expires_at, u64::from(EXPIRES));
    assert!(invite.multi_use && invite.mapped && invite.mapped_verified && invite.second_router);
    assert_eq!(invite.mapping, Mapping::Hard);
    assert_eq!(invite.candidates.len(), 5);
    assert_eq!(invite.candidates[3].kind, CandidateKind::Ipv6);
    assert_eq!(
        invite.candidates[3].addr,
        "[2a02:8071::2]:41000".parse::<SocketAddr>().unwrap()
    );
    assert_eq!(invite.hostname.as_deref(), Some("home.example.net"));
    assert_eq!(invite.encode(), invite_text(&body));
}

#[test]
fn invite_flags() {
    let candidates = good_candidates();
    assert_invite_opens(
        &invite_body(MAPPED | VERIFIED, &candidates, None),
        "verified mapping",
    );
    assert_invite_damaged(&invite_body(0x80, &candidates, None), "flag bit 7");
    assert_invite_damaged(&invite_body(3 << 4, &candidates, None), "mapping bits 3");
    assert_invite_damaged(
        &invite_body(VERIFIED, &candidates, None),
        "verified, not mapped",
    );
}

#[test]
fn seventeen_candidates() {
    let lan = v4(LAN, [192, 168, 1, 20], 41000);
    let sixteen = invite_body(0, &vec![lan.clone(); 16], None);
    assert_eq!(
        Invite::decode(&invite_text(&sixteen))
            .unwrap()
            .candidates
            .len(),
        16
    );
    assert_invite_damaged(&invite_body(0, &vec![lan; 17], None), "17 candidates");
}

#[test]
fn unknown_candidate_tags() {
    for kind in 4..=15 {
        assert_invite_damaged(
            &with_candidate(v4(kind, [192, 168, 1, 20], 41000)),
            &format!("kind {kind}"),
        );
    }
    for family in (0..=15u8).filter(|f| *f != 4 && *f != 6) {
        let mut four = v4(LAN, [192, 168, 1, 20], 41000);
        four[0] = family << 4;
        assert_invite_damaged(&with_candidate(four), &format!("family {family}, 4 bytes"));
        let mut sixteen = v6(VPN, "fd7a:115c:a1e0::1", 41000);
        sixteen[0] = (family << 4) | VPN;
        assert_invite_damaged(
            &with_candidate(sixteen),
            &format!("family {family}, 16 bytes"),
        );
    }
}

#[test]
fn candidate_kind_and_family() {
    assert_invite_damaged(
        &with_candidate(v6(LAN, "2a02:8071::2", 41000)),
        "Lan with IPv6",
    );
    assert_invite_damaged(
        &with_candidate(v6(PUBLIC, "2a02:8071::2", 41000)),
        "Public with IPv6",
    );
    assert_invite_damaged(
        &with_candidate(v4(IPV6, [198, 51, 100, 9], 41000)),
        "Ipv6 with IPv4",
    );
}

#[test]
fn unusable_addresses() {
    for kind in [LAN, VPN, PUBLIC] {
        for ip in [
            [0, 0, 0, 0],
            [127, 0, 0, 1],
            [127, 1, 2, 3],
            [224, 0, 0, 1],
            [239, 255, 255, 250],
            [255, 255, 255, 255],
        ] {
            assert_invite_damaged(&with_candidate(v4(kind, ip, 41000)), &format!("{ip:?}"));
        }
        assert_invite_damaged(&with_candidate(v4(kind, [198, 51, 100, 9], 0)), "v4 port 0");
    }
    for kind in [VPN, IPV6] {
        for ip in [
            "::",
            "::1",
            "ff02::1",
            "ff0e::1",
            "::ffff:10.0.0.1",
            "::ffff:198.51.100.9",
        ] {
            assert_invite_damaged(&with_candidate(v6(kind, ip, 41000)), ip);
        }
        assert_invite_damaged(&with_candidate(v6(kind, "2a02:8071::2", 0)), "v6 port 0");
    }
}

#[test]
fn public_and_ipv6_candidates() {
    for ip in [
        [10, 0, 0, 1],
        [172, 16, 0, 1],
        [172, 31, 255, 254],
        [192, 168, 1, 1],
        [100, 64, 0, 1],
        [100, 127, 255, 254],
        [169, 254, 1, 1],
        [0, 1, 2, 3],
        [240, 0, 0, 1],
        [254, 1, 2, 3],
    ] {
        assert_invite_damaged(&with_candidate(v4(PUBLIC, ip, 41000)), &format!("{ip:?}"));
        assert_invite_opens(&with_candidate(v4(LAN, ip, 41000)), &format!("Lan {ip:?}"));
        assert_invite_opens(&with_candidate(v4(VPN, ip, 41000)), &format!("Vpn {ip:?}"));
    }
    for ip in [
        [172, 15, 255, 255],
        [172, 32, 0, 1],
        [100, 63, 255, 255],
        [100, 128, 0, 1],
        [11, 0, 0, 1],
        [223, 255, 255, 254],
    ] {
        assert_invite_opens(&with_candidate(v4(PUBLIC, ip, 41000)), &format!("{ip:?}"));
    }
    for ip in [
        "fe80::1",
        "febf::1",
        "fc00::1",
        "fd7a:115c:a1e0::1",
        "::1.2.3.4",
        "64:ff9b::1.2.3.4",
    ] {
        assert_invite_damaged(&with_candidate(v6(IPV6, ip, 41000)), ip);
        assert_invite_opens(&with_candidate(v6(VPN, ip, 41000)), ip);
    }
    for ip in ["2001:db8::1", "fbff::1", "2002:c633:6409::1"] {
        assert_invite_opens(&with_candidate(v6(IPV6, ip, 41000)), ip);
    }
}

#[test]
fn bad_host_names() {
    let longest = format!("{0}.{0}.{0}.{1}", "a".repeat(63), "b".repeat(61));
    let too_long = format!("{0}.{0}.{0}.{1}", "a".repeat(63), "b".repeat(62));
    let long_label = "a".repeat(64);
    let candidates = good_candidates();
    assert_invite_opens(
        &invite_body(HAS_HOSTNAME, &candidates, Some(longest.as_bytes())),
        "253 bytes",
    );
    for name in [
        &b""[..],
        too_long.as_bytes(),
        long_label.as_bytes(),
        b"-a.example",
        b"a-.example",
        b"a..b",
        b".a",
        b"a.",
        b"a_b.example",
        b"a b",
        "b\u{fc}cher.example".as_bytes(),
        b"b\xfccher.example",
        b"\xff\xfe",
        b"1.2.3.4",
        b"127.1",
        b"0x7f000001",
        b"localhost",
        b"x.LocalHost",
    ] {
        assert_invite_damaged(
            &invite_body(HAS_HOSTNAME, &candidates, Some(name)),
            &String::from_utf8_lossy(name),
        );
    }
}

#[test]
fn invite_length() {
    let candidates = good_candidates();
    let whole = invite_body(HAS_HOSTNAME, &candidates, Some(b"home.example.net"));
    assert_invite_opens(&whole, "whole");
    let mut longer = whole.clone();
    longer.push(0);
    assert_invite_damaged(&longer, "one trailing byte");
    for len in 0..whole.len() {
        assert_invite_damaged(&whole[..len], &format!("cut to {len} bytes"));
    }
    assert_invite_damaged(
        &invite_body(HAS_HOSTNAME, &candidates, None),
        "flag, no name",
    );
    assert_invite_damaged(&invite_body(0, &candidates, Some(b"home")), "name, no flag");

    let mut count_too_high = invite_body(0, &candidates, None);
    let count_at = PREAMBLE + 1 + 32 + 8 + 16 + 4;
    count_too_high[count_at] += 1;
    assert_invite_damaged(&count_too_high, "count one higher than the candidates");
}

#[test]
fn unknown_body_version() {
    let mut body = invite_body(0, &good_candidates(), None);
    body[0] = 3;
    assert_eq!(
        Invite::decode(&invite_text(&body)),
        Err(CodeError::NewerVersion)
    );
    body[0] = 0;
    assert_eq!(Invite::decode(&invite_text(&body)), Err(CodeError::Damaged));

    let mut body = reply_body(REJOIN, None, None, None);
    body[0] = 2;
    assert_eq!(
        ReplyCode::decode(&reply_text(&body)),
        Err(CodeError::NewerVersion)
    );
    body[0] = 0;
    assert_eq!(
        ReplyCode::decode(&reply_text(&body)),
        Err(CodeError::Damaged)
    );
}

// What a build of another protocol makes may differ in everything after the preamble, and is
// still named by what the preamble says.
#[test]
fn another_protocol() {
    let later = Version {
        major: 0,
        minor: 2,
        patch: 0,
    };
    let refused = Err(CodeError::OtherVersion {
        protocol: 2,
        version: later,
    });
    for layout in [INVITE_LAYOUT, 3, 200] {
        let mut body = preamble(layout, 2, later);
        assert_eq!(Invite::decode(&invite_text(&body)), refused, "bare");
        body.extend_from_slice(b"a layout this build has never seen");
        assert_eq!(Invite::decode(&invite_text(&body)), refused, "unknown rest");
        let mut same_rest = preamble(layout, 2, later);
        same_rest.push(MAPPED);
        invite_fields(&mut same_rest, &good_candidates(), None);
        assert_eq!(
            Invite::decode(&invite_text(&same_rest)),
            refused,
            "our rest"
        );
    }
    // Only the protocol decides: an invite from another Booth version of this one opens.
    let mut patched = preamble(
        INVITE_LAYOUT,
        PROTOCOL,
        Version {
            patch: 9,
            ..VERSION
        },
    );
    patched.push(MAPPED);
    invite_fields(&mut patched, &good_candidates(), None);
    assert_invite_opens(&patched, "another version, this protocol");
    let mut zero = preamble(INVITE_LAYOUT, 0, VERSION);
    zero.push(MAPPED);
    invite_fields(&mut zero, &good_candidates(), None);
    assert_invite_damaged(&zero, "protocol 0");
}

// Exactly what the test builds before 0.1.0 sent: the fields with no preamble in front.
#[test]
fn unversioned_invite() {
    let mut old = vec![1, MAPPED | HAS_HOSTNAME];
    invite_fields(&mut old, &good_candidates(), Some(b"home.example.net"));
    assert_eq!(
        Invite::decode(&invite_text(&old)),
        Err(CodeError::Unversioned)
    );
}

fn reply_body(
    flags: u8,
    invite_id: Option<[u8; 8]>,
    outside_v4: Option<([u8; 4], u16)>,
    outside_v6: Option<(&str, u16)>,
) -> Vec<u8> {
    let mut body = vec![REPLY_LAYOUT, flags];
    if let Some(id) = invite_id {
        body.extend_from_slice(&id);
    }
    body.extend_from_slice(&[9; 32]);
    if let Some((ip, port)) = outside_v4 {
        body.extend_from_slice(&ip);
        body.extend_from_slice(&port.to_be_bytes());
    }
    if let Some((ip, port)) = outside_v6 {
        body.extend_from_slice(&ip.parse::<Ipv6Addr>().unwrap().octets());
        body.extend_from_slice(&port.to_be_bytes());
    }
    body.extend_from_slice(&EXPIRES.to_be_bytes());
    body
}

fn full_reply() -> Vec<u8> {
    reply_body(
        HAS_V4 | HAS_V6 | REPLY_EASY,
        Some([1, 2, 3, 4, 5, 6, 7, 8]),
        Some(([198, 51, 100, 7], 52311)),
        Some(("2001:db8::7", 52311)),
    )
}

fn assert_reply_damaged(body: &[u8], what: &str) {
    assert_eq!(
        ReplyCode::decode(&reply_text(body)),
        Err(CodeError::Damaged),
        "{what}"
    );
}

#[test]
fn hand_built_reply_code() {
    let body = full_reply();
    let code = ReplyCode::decode(&reply_text(&body)).unwrap();
    assert_eq!(code.answers, Answers::Invite([1, 2, 3, 4, 5, 6, 7, 8]));
    assert_eq!(code.client_key, [9; 32]);
    assert_eq!(code.outside_v4, Some("198.51.100.7:52311".parse().unwrap()));
    assert_eq!(
        code.outside_v6,
        Some("[2001:db8::7]:52311".parse().unwrap())
    );
    assert_eq!(code.mapping, Mapping::Easy);
    assert_eq!(code.expires_at, u64::from(EXPIRES));
    assert_eq!(code.encode(), reply_text(&body));

    let rejoin = reply_body(REJOIN, None, None, None);
    let code = ReplyCode::decode(&reply_text(&rejoin)).unwrap();
    assert_eq!(code.answers, Answers::Rejoin);
    assert_eq!(code.mapping, Mapping::Unknown);
    assert_eq!(code.encode(), reply_text(&rejoin));
}

#[test]
fn reply_flags() {
    for bit in [1 << 5, 1 << 6, 1 << 7] {
        assert_reply_damaged(
            &reply_body(REJOIN | bit, None, None, None),
            &format!("{bit:#x}"),
        );
    }
    assert_reply_damaged(
        &reply_body(REJOIN | (3 << 3), None, None, None),
        "mapping bits 3",
    );
}

#[test]
fn reply_addresses() {
    for ip in [
        [127, 0, 0, 1],
        [0, 0, 0, 0],
        [224, 0, 0, 1],
        [255, 255, 255, 255],
        [192, 168, 1, 1],
        [10, 0, 0, 1],
        [172, 16, 0, 1],
        [100, 64, 0, 1],
        [169, 254, 1, 1],
        [0, 1, 2, 3],
        [240, 0, 0, 1],
    ] {
        let body = reply_body(REJOIN | HAS_V4, None, Some((ip, 52311)), None);
        assert_reply_damaged(&body, &format!("{ip:?}"));
    }
    let port_zero = reply_body(REJOIN | HAS_V4, None, Some(([198, 51, 100, 7], 0)), None);
    assert_reply_damaged(&port_zero, "v4 port 0");
    for ip in [
        "::",
        "::1",
        "ff02::1",
        "::ffff:10.0.0.1",
        "fe80::1",
        "fd00::1",
        "::1.2.3.4",
        "64:ff9b::1.2.3.4",
    ] {
        let body = reply_body(REJOIN | HAS_V6, None, None, Some((ip, 52311)));
        assert_reply_damaged(&body, ip);
    }
    let port_zero = reply_body(REJOIN | HAS_V6, None, None, Some(("2001:db8::7", 0)));
    assert_reply_damaged(&port_zero, "v6 port 0");
}

#[test]
fn reply_length() {
    let whole = full_reply();
    let mut longer = whole.clone();
    longer.push(0);
    assert_reply_damaged(&longer, "one trailing byte");
    for len in 0..whole.len() {
        assert_reply_damaged(&whole[..len], &format!("cut to {len} bytes"));
    }
    let id = Some([1, 2, 3, 4, 5, 6, 7, 8]);
    assert_reply_damaged(
        &reply_body(REJOIN, id, None, None),
        "rejoin with an invite id",
    );
    assert_reply_damaged(&reply_body(0, None, None, None), "no invite id");
    let v4 = Some(([198, 51, 100, 7], 52311));
    assert_reply_damaged(&reply_body(REJOIN, None, v4, None), "address, no flag");
    assert_reply_damaged(
        &reply_body(REJOIN | HAS_V4, None, None, None),
        "flag, no address",
    );
}
