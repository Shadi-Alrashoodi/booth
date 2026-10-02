use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use invite::{
    Answers, BuildError, Candidate, CandidateKind, CodeError, Invite, MULTI_USE_SECS, Mapping,
    PROTOCOL, REPLY_SECS, ReplyCode, SINGLE_USE_SECS, VERSION, Version, check_addr, check_hostname,
};

const NOW: u64 = 1_790_000_000;

fn v4(a: u8, b: u8, c: u8, d: u8, port: u16) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(a, b, c, d), port))
}

fn v6(text: &str, port: u16) -> SocketAddr {
    SocketAddr::V6(SocketAddrV6::new(text.parse().unwrap(), port, 0, 0))
}

fn candidate(kind: CandidateKind, addr: SocketAddr) -> Candidate {
    Candidate { kind, addr }
}

// What a host at home with Tailscale, WireGuard, IPv6 and a working STUN answer puts in.
fn typical_candidates() -> Vec<Candidate> {
    vec![
        candidate(CandidateKind::Lan, v4(192, 168, 178, 23, 41000)),
        candidate(CandidateKind::Vpn, v4(100, 101, 102, 103, 41000)),
        candidate(CandidateKind::Vpn, v4(10, 8, 0, 2, 41000)),
        candidate(
            CandidateKind::Ipv6,
            v6("2a02:8071:1234:5600:8d3c:1a2b:3c4d:5e6f", 41000),
        ),
        candidate(CandidateKind::Public, v4(203, 0, 113, 45, 41000)),
    ]
}

fn fixed_invite() -> Invite {
    Invite {
        host_key: [7; 32],
        invite_id: [1, 2, 3, 4, 5, 6, 7, 8],
        secret: [0xab; 16],
        multi_use: false,
        expires_at: NOW + SINGLE_USE_SECS,
        candidates: typical_candidates(),
        mapping: Mapping::Easy,
        mapped: true,
        mapped_verified: false,
        second_router: false,
        hostname: None,
    }
}

fn fixed_reply() -> ReplyCode {
    ReplyCode {
        answers: Answers::Invite([1, 2, 3, 4, 5, 6, 7, 8]),
        client_key: [9; 32],
        outside_v4: Some(SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 7), 52311)),
        outside_v6: None,
        mapping: Mapping::Easy,
        expires_at: NOW + REPLY_SECS,
    }
}

#[test]
fn typical_invite_round_trips() {
    let invite = Invite::new(
        [7; 32],
        typical_candidates(),
        Mapping::Easy,
        true,
        false,
        false,
        None,
        false,
        NOW,
    )
    .unwrap();
    assert_eq!(Invite::decode(&invite.encode()).unwrap(), invite);
}

#[test]
fn full_invite_round_trips() {
    let candidates = vec![
        candidate(CandidateKind::Vpn, v6("fd7a:115c:a1e0::1234", 41000)),
        candidate(CandidateKind::Ipv6, v6("2001:db8::1", 65535)),
        candidate(CandidateKind::Lan, v4(169, 254, 10, 1, 1)),
    ];
    for mapping in [Mapping::Easy, Mapping::Hard, Mapping::Unknown] {
        let invite = Invite::new(
            [0xfe; 32],
            candidates.clone(),
            mapping,
            true,
            true,
            true,
            Some("Home-PC.dyn.example.net".into()),
            true,
            NOW,
        )
        .unwrap();
        let decoded = Invite::decode(&invite.encode()).unwrap();
        assert_eq!(decoded, invite);
        assert!(decoded.multi_use && decoded.mapped && decoded.mapped_verified);
        assert!(decoded.second_router);
        assert_eq!(decoded.hostname.as_deref(), Some("Home-PC.dyn.example.net"));
    }
}

#[test]
fn invite_with_only_a_hostname_round_trips() {
    let invite = Invite::new(
        [3; 32],
        Vec::new(),
        Mapping::Unknown,
        false,
        false,
        false,
        Some("a".into()),
        false,
        NOW,
    )
    .unwrap();
    assert_eq!(Invite::decode(&invite.encode()).unwrap(), invite);
}

#[test]
fn new_invites_get_fresh_random_ids_and_secrets() {
    let make = || {
        Invite::new(
            [7; 32],
            typical_candidates(),
            Mapping::Easy,
            false,
            false,
            false,
            None,
            false,
            NOW,
        )
        .unwrap()
    };
    let (a, b) = (make(), make());
    assert_ne!(a.invite_id, b.invite_id);
    assert_ne!(a.secret, b.secret);
    assert_ne!(a.secret, [0; 16]);
}

#[test]
fn scope_id_is_dropped() {
    let with_scope = SocketAddr::V6(SocketAddrV6::new(
        "2001:db8::5".parse().unwrap(),
        41000,
        7,
        12,
    ));
    let invite = Invite::new(
        [7; 32],
        vec![candidate(CandidateKind::Ipv6, with_scope)],
        Mapping::Easy,
        false,
        false,
        false,
        None,
        false,
        NOW,
    )
    .unwrap();
    assert_eq!(invite.candidates[0].addr, v6("2001:db8::5", 41000));
    assert_eq!(Invite::decode(&invite.encode()).unwrap(), invite);
}

// check() is the promise that a value comes back unchanged, and a code cannot carry a scope id
// or flow label, so a value built without new() that has one must fail check().
#[test]
fn check_refuses_a_scope_id_or_flow_label() {
    for (flow, scope) in [(0, 9), (5, 0), (5, 9)] {
        let scoped = SocketAddrV6::new("2a02:8071::1".parse().unwrap(), 41000, flow, scope);
        let mut invite = fixed_invite();
        invite.candidates = vec![candidate(CandidateKind::Ipv6, SocketAddr::V6(scoped))];
        assert!(
            matches!(invite.check(), Err(BuildError::BadCandidate { .. })),
            "{scoped:?}"
        );
        invite.candidates = vec![candidate(CandidateKind::Vpn, SocketAddr::V6(scoped))];
        assert!(invite.check().is_err(), "{scoped:?}");

        let mut code = fixed_reply();
        code.outside_v6 = Some(scoped);
        assert!(
            matches!(code.check(), Err(BuildError::BadAddress { .. })),
            "{scoped:?}"
        );
        let code = ReplyCode::new(
            Answers::Rejoin,
            [1; 32],
            None,
            Some(scoped),
            Mapping::Easy,
            NOW,
        )
        .unwrap();
        assert_eq!(ReplyCode::decode(&code.encode()), Ok(code));
    }
}

#[test]
fn verified_needs_a_mapping() {
    let err = Invite::new(
        [7; 32],
        typical_candidates(),
        Mapping::Easy,
        false,
        true,
        false,
        None,
        false,
        NOW,
    )
    .unwrap_err();
    assert_eq!(err, BuildError::VerifiedWithoutMapping);
    let mut invite = fixed_invite();
    invite.mapped = false;
    invite.mapped_verified = true;
    assert_eq!(invite.check(), Err(BuildError::VerifiedWithoutMapping));
}

#[test]
fn reply_code_answering_an_invite_round_trips() {
    let code = ReplyCode::new(
        Answers::Invite([8, 7, 6, 5, 4, 3, 2, 1]),
        [0x5a; 32],
        Some(SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 7), 52311)),
        Some(SocketAddrV6::new(
            "2003:e2:1f00::99".parse().unwrap(),
            52311,
            0,
            0,
        )),
        Mapping::Easy,
        NOW,
    )
    .unwrap();
    assert_eq!(ReplyCode::decode(&code.encode()).unwrap(), code);
}

#[test]
fn reply_code_for_a_rejoin_round_trips() {
    for (v4, v6) in [
        (
            Some(SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 7), 1)),
            None,
        ),
        (
            None,
            Some(SocketAddrV6::new("2001:db8::7".parse().unwrap(), 2, 0, 0)),
        ),
        (None, None),
    ] {
        for mapping in [Mapping::Easy, Mapping::Hard, Mapping::Unknown] {
            let code = ReplyCode::new(Answers::Rejoin, [0x77; 32], v4, v6, mapping, NOW).unwrap();
            assert_eq!(ReplyCode::decode(&code.encode()).unwrap(), code);
        }
    }
}

#[test]
fn typical_invite_length() {
    let text = fixed_invite().encode();
    println!("typical invite: {} characters: {text}", text.len());
    // 13 characters of it are the protocol and version, since 0.1.0.
    assert!(
        (150..=205).contains(&text.len()),
        "{} characters",
        text.len()
    );
    let reply = fixed_reply().encode();
    println!("typical reply code: {} characters: {reply}", reply.len());
}

#[test]
fn changed_character() {
    let text = fixed_invite().encode();
    let alphabet = "abcdefghijklmnopqrstuvwxyz234567";
    let body_start = "booth1-".len();
    for (i, original) in text.char_indices().skip(body_start) {
        for replacement in alphabet.chars().filter(|&c| c != original) {
            let mut changed = text.clone();
            changed.replace_range(i..i + 1, &replacement.to_string());
            assert_eq!(
                Invite::decode(&changed),
                Err(CodeError::Damaged),
                "position {i}: {original} to {replacement}"
            );
        }
    }
}

#[test]
fn dropped_or_doubled_character() {
    let text = fixed_invite().encode();
    for i in "booth1-".len()..text.len() {
        let mut dropped = text.clone();
        dropped.remove(i);
        assert_eq!(Invite::decode(&dropped), Err(CodeError::Damaged));
        let mut doubled = text.clone();
        doubled.insert(i, text.as_bytes()[i] as char);
        assert_eq!(Invite::decode(&doubled), Err(CodeError::Damaged));
    }
}

// Five host name lengths in a row give every possible number of padding bits at the end, and on
// some of them an added 'a' carries only zero bits and completes no byte.
#[test]
fn symbol_added_at_the_end() {
    for len in 1..=5 {
        let invite = Invite::new(
            [7; 32],
            typical_candidates(),
            Mapping::Easy,
            false,
            false,
            false,
            Some("h".repeat(len)),
            false,
            NOW,
        )
        .unwrap();
        let text = invite.encode();
        assert_eq!(Invite::decode(&text).as_ref(), Ok(&invite));
        for extra in ["a", "aa", "aaa", "q", "7"] {
            assert_eq!(
                Invite::decode(&format!("{text}{extra}")),
                Err(CodeError::Damaged),
                "host name length {len}, plus {extra}"
            );
        }
    }
}

#[test]
fn pasted_noise_is_tolerated() {
    let invite = fixed_invite();
    let text = invite.encode();
    let wrapped: String = text
        .as_bytes()
        .chunks(40)
        .map(|chunk| std::str::from_utf8(chunk).unwrap())
        .collect::<Vec<_>>()
        .join("\r\n");
    let variants = [
        text.to_uppercase(),
        format!("  {text}  "),
        format!("\"{text}\"."),
        format!("'{text}';"),
        format!("<{text}>,"),
        format!("`{text}`"),
        format!("\u{201c}{text}\u{201d}"),
        format!("{text}."),
        format!("{text} .\n"),
        format!("\t{wrapped}\n"),
        format!("\"{}\"", wrapped.to_uppercase()),
        text.replacen("booth1-", "Booth1-", 1),
        text.replace('a', "a\u{200b}"),
        text.replace('b', "b\u{a0}"),
        // Direction marks and isolates that chat apps wrap around pasted text.
        format!("\u{200e}{text}\u{200f}"),
        format!("\u{202a}{text}\u{202c}"),
        format!("\u{202b}\u{202d}{text}\u{202e}\u{202c}"),
        format!("\u{2066}{text}\u{2069}"),
        format!("\u{2067}\u{2068}{text}\u{2069}\u{2069}."),
        text.replace('c', "c\u{200e}"),
        text.replacen("booth1-", "booth1\u{2011}", 1),
        text.replacen("booth1-", "booth1\u{2010}", 1),
    ];
    for variant in variants {
        assert_eq!(
            Invite::decode(&variant).as_ref(),
            Ok(&invite),
            "{variant:?}"
        );
    }

    let code = fixed_reply();
    let reply = code.encode();
    for variant in [
        reply.to_uppercase(),
        format!("\"{reply}\"."),
        reply.replacen("booth1-r-", "BOOTH1-R-", 1),
        reply.replace('c', "c\n"),
    ] {
        assert_eq!(ReplyCode::decode(&variant), Ok(code), "{variant:?}");
    }
}

#[test]
fn noise_inside_the_code_is_damage() {
    let text = fixed_invite().encode();
    for noise in [".", ",", "\"", "-", "=", "0", "1", "8", "9", "\u{e9}"] {
        let mut changed = text.clone();
        changed.insert_str(30, noise);
        assert_eq!(
            Invite::decode(&changed),
            Err(CodeError::Damaged),
            "{noise:?}"
        );
    }
    assert_eq!(
        Invite::decode(&format!("{text}==")),
        Err(CodeError::Damaged)
    );
}

#[test]
fn code_in_the_wrong_field() {
    let invite = fixed_invite().encode();
    let reply = fixed_reply().encode();
    assert_eq!(Invite::decode(&reply), Err(CodeError::IsReplyCode));
    assert_eq!(
        Invite::decode(&reply.to_uppercase()),
        Err(CodeError::IsReplyCode)
    );
    assert_eq!(ReplyCode::decode(&invite), Err(CodeError::IsInvite));
    assert_eq!(
        ReplyCode::decode(&format!(" \"{invite}\". ")),
        Err(CodeError::IsInvite)
    );
}

#[test]
fn body_under_the_wrong_prefix() {
    let invite = fixed_invite().encode();
    let reply = fixed_reply().encode();
    let invite_body = invite.strip_prefix("booth1-").unwrap();
    let reply_body = reply.strip_prefix("booth1-r-").unwrap();
    assert_eq!(
        ReplyCode::decode(&format!("booth1-r-{invite_body}")),
        Err(CodeError::Damaged)
    );
    assert_eq!(
        Invite::decode(&format!("booth1-{reply_body}")),
        Err(CodeError::Damaged)
    );
}

#[test]
fn text_that_is_not_a_code() {
    for text in ["", "   ", "\r\n\t", "\"\"", "<>", "\u{200b}", "."] {
        assert_eq!(Invite::decode(text), Err(CodeError::Empty), "{text:?}");
        assert_eq!(ReplyCode::decode(text), Err(CodeError::Empty), "{text:?}");
    }
    for text in [
        "hello",
        "booth",
        "booth-abc",
        "boothabc",
        "booth1",
        "booth1abc",
        "booth0-abc",
        "https://example.com/booth1-abc",
        "join me: booth1-abc",
        "b\u{e9}\u{e9}th1-abc",
        "booth1\u{2013}abc",
    ] {
        assert_eq!(Invite::decode(text), Err(CodeError::NotACode), "{text:?}");
        assert_eq!(
            ReplyCode::decode(text),
            Err(CodeError::NotACode),
            "{text:?}"
        );
    }
}

// booth1 has one spelling. A leading zero is not a newer version either.
#[test]
fn leading_zero_version() {
    let invite = fixed_invite().encode();
    let reply = fixed_reply().encode();
    for zeros in ["0", "00", "000"] {
        for version in ["1", "2"] {
            let spelled = format!("booth{zeros}{version}-");
            let text = invite.replacen("booth1-", &spelled, 1);
            assert_eq!(Invite::decode(&text), Err(CodeError::NotACode), "{text}");
            let text = reply.replacen("booth1-", &spelled, 1);
            assert_eq!(ReplyCode::decode(&text), Err(CodeError::NotACode), "{text}");
        }
    }
}

#[test]
fn codes_from_newer_versions() {
    for text in [
        "booth2-aaaa",
        "BOOTH2-R-aaaa",
        "booth10-",
        "booth99999999999999999999999-x",
        &format!("booth3-{}", "a".repeat(5000)),
    ] {
        assert_eq!(Invite::decode(text), Err(CodeError::NewerVersion), "{text}");
        assert_eq!(
            ReplyCode::decode(text),
            Err(CodeError::NewerVersion),
            "{text}"
        );
    }
}

#[test]
fn oversized_and_truncated_codes_are_damage() {
    let text = fixed_invite().encode();
    assert_eq!(
        Invite::decode(&format!("booth1-{}", "a".repeat(100_000))),
        Err(CodeError::Damaged)
    );
    assert_eq!(Invite::decode("booth1-"), Err(CodeError::Damaged));
    assert_eq!(ReplyCode::decode("booth1-r-"), Err(CodeError::Damaged));
    for len in "booth1-".len()..text.len() {
        assert_eq!(
            Invite::decode(&text[..len]),
            Err(CodeError::Damaged),
            "{len}"
        );
    }
}

#[test]
fn invite_expiry() {
    let single = Invite::new(
        [7; 32],
        typical_candidates(),
        Mapping::Easy,
        false,
        false,
        false,
        None,
        false,
        NOW,
    )
    .unwrap();
    assert_eq!(single.expires_at, NOW + 10 * 60);
    assert!(!single.is_expired(NOW));
    assert!(!single.is_expired(NOW + 10 * 60 - 1));
    assert!(single.is_expired(NOW + 10 * 60));
    assert!(single.is_expired(u64::MAX));

    let multi = Invite::new(
        [7; 32],
        typical_candidates(),
        Mapping::Easy,
        false,
        false,
        false,
        None,
        true,
        NOW,
    )
    .unwrap();
    assert_eq!(multi.expires_at, NOW + MULTI_USE_SECS);
    assert!(!multi.is_expired(NOW + 24 * 3600 - 1));
    assert!(multi.is_expired(NOW + 24 * 3600));

    let decoded = Invite::decode(&single.encode()).unwrap();
    assert!(!decoded.is_expired(NOW + 10 * 60 - 1));
    assert!(decoded.is_expired(NOW + 10 * 60));
}

#[test]
fn reply_code_expiry() {
    let code = ReplyCode::new(
        Answers::Rejoin,
        [1; 32],
        Some(SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 7), 5000)),
        None,
        Mapping::Unknown,
        NOW,
    )
    .unwrap();
    assert_eq!(code.expires_at, NOW + 5 * 60);
    assert!(!code.is_expired(NOW + 5 * 60 - 1));
    assert!(code.is_expired(NOW + 5 * 60));
    let decoded = ReplyCode::decode(&code.encode()).unwrap();
    assert!(decoded.is_expired(NOW + 5 * 60));
}

#[test]
fn expiry_past_2106() {
    let late = u64::from(u32::MAX) - 60;
    let invite = Invite::new(
        [7; 32],
        Vec::new(),
        Mapping::Easy,
        false,
        false,
        false,
        None,
        false,
        late,
    );
    assert!(matches!(invite, Err(BuildError::ExpiryOutOfRange(_))));
    let code = ReplyCode::new(
        Answers::Rejoin,
        [1; 32],
        None,
        None,
        Mapping::Easy,
        u64::MAX,
    );
    assert!(matches!(code, Err(BuildError::ExpiryOutOfRange(_))));

    let last = u64::from(u32::MAX);
    let code = ReplyCode::new(
        Answers::Rejoin,
        [1; 32],
        None,
        None,
        Mapping::Easy,
        last - REPLY_SECS,
    )
    .unwrap();
    assert_eq!(ReplyCode::decode(&code.encode()).unwrap().expires_at, last);
}

#[test]
fn bad_candidates() {
    let bad = [
        candidate(CandidateKind::Lan, v4(127, 0, 0, 1, 41000)),
        candidate(CandidateKind::Lan, v4(0, 0, 0, 0, 41000)),
        candidate(CandidateKind::Lan, v4(239, 1, 2, 3, 41000)),
        candidate(CandidateKind::Public, v4(255, 255, 255, 255, 41000)),
        candidate(CandidateKind::Lan, v4(192, 168, 1, 2, 0)),
        candidate(CandidateKind::Lan, v6("2001:db8::1", 41000)),
        candidate(CandidateKind::Public, v6("2001:db8::1", 41000)),
        candidate(CandidateKind::Ipv6, v4(192, 168, 1, 2, 41000)),
        candidate(CandidateKind::Ipv6, v6("::1", 41000)),
        candidate(CandidateKind::Ipv6, v6("::", 41000)),
        candidate(CandidateKind::Vpn, v6("ff02::1", 41000)),
        candidate(CandidateKind::Ipv6, v6("::ffff:192.168.1.2", 41000)),
        candidate(CandidateKind::Public, v4(192, 168, 1, 1, 41000)),
        candidate(CandidateKind::Public, v4(10, 0, 0, 1, 41000)),
        candidate(CandidateKind::Public, v4(172, 16, 0, 1, 41000)),
        candidate(CandidateKind::Public, v4(100, 64, 0, 1, 41000)),
        candidate(CandidateKind::Public, v4(169, 254, 1, 1, 41000)),
        candidate(CandidateKind::Public, v4(0, 1, 2, 3, 41000)),
        candidate(CandidateKind::Public, v4(240, 0, 0, 1, 41000)),
        candidate(CandidateKind::Ipv6, v6("fe80::1", 41000)),
        candidate(CandidateKind::Ipv6, v6("fd00::1", 41000)),
        candidate(CandidateKind::Ipv6, v6("::1.2.3.4", 41000)),
        candidate(CandidateKind::Ipv6, v6("64:ff9b::1.2.3.4", 41000)),
    ];
    for c in bad {
        let err = Invite::new(
            [7; 32],
            vec![c],
            Mapping::Easy,
            false,
            false,
            false,
            None,
            false,
            NOW,
        )
        .unwrap_err();
        assert!(matches!(err, BuildError::BadCandidate { .. }), "{c:?}");
        println!("{err}");
    }

    let too_many = vec![candidate(CandidateKind::Lan, v4(192, 168, 1, 2, 41000)); 17];
    let err = Invite::new(
        [7; 32],
        too_many,
        Mapping::Easy,
        false,
        false,
        false,
        None,
        false,
        NOW,
    )
    .unwrap_err();
    assert_eq!(err, BuildError::TooManyCandidates(17));

    let err = ReplyCode::new(
        Answers::Rejoin,
        [1; 32],
        Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5000)),
        None,
        Mapping::Easy,
        NOW,
    )
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "cannot put 127.0.0.1:5000 in a reply code: it is a loopback address"
    );
    let err = ReplyCode::new(
        Answers::Rejoin,
        [1; 32],
        None,
        Some(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 5000, 0, 0)),
        Mapping::Easy,
        NOW,
    )
    .unwrap_err();
    assert!(matches!(err, BuildError::BadAddress { .. }));

    for (v4, v6) in [
        (
            Some(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 1), 80)),
            None,
        ),
        (
            Some(SocketAddrV4::new(Ipv4Addr::new(100, 64, 0, 1), 5000)),
            None,
        ),
        (
            None,
            Some(SocketAddrV6::new("fe80::1".parse().unwrap(), 5000, 0, 0)),
        ),
        (
            None,
            Some(SocketAddrV6::new("fd00::1".parse().unwrap(), 5000, 0, 0)),
        ),
    ] {
        let err = ReplyCode::new(Answers::Rejoin, [1; 32], v4, v6, Mapping::Easy, NOW).unwrap_err();
        assert!(matches!(err, BuildError::BadAddress { .. }), "{err}");
        println!("{err}");
    }
}

// Tailscale hands out 100.64/10 and fd7a:115c:a1e0::/48, and a LAN can be anything private, so
// only Public and Ipv6 candidates have to be internet addresses.
#[test]
fn private_addresses_are_fine_on_lan_and_vpn() {
    let candidates = vec![
        candidate(CandidateKind::Lan, v4(10, 0, 0, 5, 41000)),
        candidate(CandidateKind::Lan, v4(169, 254, 3, 4, 41000)),
        candidate(CandidateKind::Vpn, v4(100, 100, 1, 2, 41000)),
        candidate(CandidateKind::Vpn, v4(172, 20, 0, 2, 41000)),
        candidate(CandidateKind::Vpn, v6("fd7a:115c:a1e0::1", 41000)),
        candidate(CandidateKind::Vpn, v6("fe80::1234", 41000)),
    ];
    let invite = Invite::new(
        [7; 32],
        candidates,
        Mapping::Easy,
        false,
        false,
        false,
        None,
        false,
        NOW,
    )
    .unwrap();
    assert_eq!(Invite::decode(&invite.encode()).unwrap(), invite);
}

#[test]
fn check_addr_rules() {
    for bad in [
        v4(127, 0, 0, 1, 41000),
        v4(0, 0, 0, 0, 41000),
        v4(224, 0, 0, 1, 41000),
        v4(255, 255, 255, 255, 41000),
        v4(198, 51, 100, 7, 0),
        v6("::1", 41000),
        v6("::", 41000),
        v6("ff02::1", 41000),
        v6("::ffff:127.0.0.1", 41000),
    ] {
        assert!(check_addr(bad).is_err(), "{bad}");
    }
    for good in [
        v4(192, 168, 1, 20, 41000),
        v4(100, 64, 0, 1, 41000),
        v4(198, 51, 100, 7, 41000),
        v6("fd7a:115c:a1e0::1", 41000),
        v6("2a02:8071::1", 41000),
    ] {
        assert_eq!(check_addr(good), Ok(()), "{good}");
    }
}

#[test]
fn hostname_rules() {
    let longest = format!("{0}.{0}.{0}.{1}", "a".repeat(63), "b".repeat(61));
    assert_eq!(longest.len(), 253);
    for good in [
        "a",
        "localhost-free.example",
        "localhost.example",
        "HOME.Example.NET",
        "1.2.3.example",
        "a1.b2",
        "host.0xg",
        "xn--bcher-kva.example",
        &longest,
    ] {
        assert_eq!(check_hostname(good), Ok(()), "{good}");
    }
    let too_long = format!("{longest}a");
    let long_label = "a".repeat(64);
    for bad in [
        "",
        ".",
        "example.com.",
        ".example.com",
        "a..b",
        "-a.example",
        "a-.example",
        "under_score.example",
        "sp ace.example",
        "b\u{fc}cher.example",
        &too_long,
        &long_label,
        // Windows reads all of these as IPv4 addresses, and localhost as this PC.
        "1.2.3.4",
        "127.0.0.1",
        "127.1",
        "2130706433",
        "0x7f000001",
        "0X7F.0.0.1",
        "host.0x",
        "example.123",
        "localhost",
        "printer.LocalHost",
    ] {
        assert!(check_hostname(bad).is_err(), "{bad:?}");
        let invite = Invite::new(
            [7; 32],
            Vec::new(),
            Mapping::Easy,
            false,
            false,
            false,
            Some(bad.to_string()),
            false,
            NOW,
        );
        assert!(matches!(invite, Err(BuildError::BadHostname(_))), "{bad:?}");
    }
    assert_eq!(
        check_hostname("my_pc.example").unwrap_err().to_string(),
        "the host name may contain only letters, digits, hyphens and dots"
    );
}

#[test]
fn debug_output_hides_the_secret() {
    let shown = format!("{:?}", fixed_invite());
    assert!(shown.contains("hidden"), "{shown}");
    assert!(!shown.contains("abab"), "{shown}");
    assert!(!shown.contains("171"), "{shown}");
    let pretty = format!("{:#?}", fixed_invite());
    assert!(
        !pretty.contains("abab") && !pretty.contains("171"),
        "{pretty}"
    );
}

#[test]
fn error_messages() {
    assert_eq!(
        CodeError::IsReplyCode.to_string(),
        "this is a reply code; paste it on the host's screen, not in Join"
    );
    assert_eq!(
        CodeError::Damaged.to_string(),
        "the code is damaged; copy it again from the message you were sent"
    );
    assert_eq!(
        CodeError::NewerVersion.to_string(),
        "this code was made by a newer version of Booth; update Booth and paste it again"
    );
    let newer = Version {
        major: 7,
        minor: 1,
        patch: 0,
    };
    assert_eq!(
        CodeError::OtherVersion {
            protocol: PROTOCOL + 1,
            version: newer
        }
        .to_string(),
        format!("this invite is for Booth 7.1.0 and you have {VERSION}")
    );
    assert_eq!(
        CodeError::OtherVersion {
            protocol: PROTOCOL + 1,
            version: VERSION
        }
        .to_string(),
        format!(
            "this invite is for Booth {VERSION} (protocol {}) and you have {VERSION} (protocol {PROTOCOL})",
            PROTOCOL + 1
        )
    );
    assert_eq!(
        CodeError::Unversioned.to_string(),
        "this invite is from a test build of Booth made before the first release"
    );
}

// Only a release build gets here with an invalid invite; a debug build stops at the assert.
#[cfg(not(debug_assertions))]
#[test]
fn release_encode_drops_bad_parts() {
    let mut invite = fixed_invite();
    invite
        .candidates
        .push(candidate(CandidateKind::Lan, v4(127, 0, 0, 1, 41000)));
    invite.candidates.extend(std::iter::repeat_n(
        candidate(CandidateKind::Lan, v4(10, 0, 0, 9, 41000)),
        20,
    ));
    invite.candidates.insert(
        1,
        candidate(
            CandidateKind::Ipv6,
            SocketAddr::V6(SocketAddrV6::new(
                "2a02:8071::1".parse().unwrap(),
                41000,
                0,
                3,
            )),
        ),
    );
    invite.hostname = Some("bad_name".into());
    invite.expires_at = u64::MAX;
    invite.mapped = false;
    invite.mapped_verified = true;
    let decoded = Invite::decode(&invite.encode()).unwrap();
    assert_eq!(decoded.candidates.len(), 16);
    assert_eq!(decoded.candidates[..5], typical_candidates()[..]);
    assert_eq!(decoded.hostname, None);
    assert_eq!(decoded.expires_at, u64::from(u32::MAX));
    assert!(!decoded.mapped && !decoded.mapped_verified);

    let mut code = fixed_reply();
    code.outside_v4 = Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
    code.outside_v6 = Some(SocketAddrV6::new("fd00::1".parse().unwrap(), 5000, 0, 0));
    let decoded = ReplyCode::decode(&code.encode()).unwrap();
    assert_eq!(decoded.outside_v4, None);
    assert_eq!(decoded.outside_v6, None);
}
