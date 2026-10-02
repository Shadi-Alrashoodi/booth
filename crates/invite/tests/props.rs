use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use invite::{Answers, Candidate, CandidateKind, Invite, Mapping, ReplyCode};
use proptest::collection::vec;
use proptest::option;
use proptest::prelude::*;

// The address rules are written out again here instead of borrowed from the crate, so a change
// to either side shows up as a failure.
fn usable_v4() -> impl Strategy<Value = SocketAddrV4> {
    (any::<[u8; 4]>(), 1..=u16::MAX)
        .prop_map(|(ip, port)| SocketAddrV4::new(Ipv4Addr::from(ip), port))
        .prop_filter("usable IPv4", |a| {
            let ip = a.ip();
            !(ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() || ip.is_broadcast())
        })
}

fn usable_v6() -> impl Strategy<Value = SocketAddrV6> {
    (any::<[u8; 16]>(), 1..=u16::MAX)
        .prop_map(|(ip, port)| SocketAddrV6::new(Ipv6Addr::from(ip), port, 0, 0))
        .prop_filter("usable IPv6", |a| {
            let ip = a.ip();
            !(ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_multicast()
                || ip.to_ipv4_mapped().is_some())
        })
}

fn internet_v4() -> impl Strategy<Value = SocketAddrV4> {
    usable_v4().prop_filter("reachable from the internet", |a| {
        let [a, b, ..] = a.ip().octets();
        let private = a == 10 || (a == 172 && (16..32).contains(&b)) || (a == 192 && b == 168);
        let cgnat = a == 100 && (64..128).contains(&b);
        !(private || cgnat || (a == 169 && b == 254) || a == 0 || a >= 240)
    })
}

fn internet_v6() -> impl Strategy<Value = SocketAddrV6> {
    usable_v6().prop_filter("reachable from the internet", |a| {
        let s = a.ip().segments();
        let link_local = s[0] & 0xffc0 == 0xfe80;
        let unique_local = s[0] & 0xfe00 == 0xfc00;
        let compat = s[..6] == [0; 6];
        let nat64 = s[..6] == [0x64, 0xff9b, 0, 0, 0, 0];
        !(link_local || unique_local || compat || nat64)
    })
}

fn usable_candidate() -> impl Strategy<Value = Candidate> {
    prop_oneof![
        usable_v4().prop_map(|a| (CandidateKind::Lan, SocketAddr::V4(a))),
        internet_v4().prop_map(|a| (CandidateKind::Public, SocketAddr::V4(a))),
        usable_v4().prop_map(|a| (CandidateKind::Vpn, SocketAddr::V4(a))),
        usable_v6().prop_map(|a| (CandidateKind::Vpn, SocketAddr::V6(a))),
        internet_v6().prop_map(|a| (CandidateKind::Ipv6, SocketAddr::V6(a))),
    ]
    .prop_map(|(kind, addr)| Candidate { kind, addr })
}

fn any_candidate() -> impl Strategy<Value = Candidate> {
    let kind = prop_oneof![
        Just(CandidateKind::Lan),
        Just(CandidateKind::Vpn),
        Just(CandidateKind::Ipv6),
        Just(CandidateKind::Public),
    ];
    let addr = prop_oneof![
        (any::<[u8; 4]>(), any::<u16>())
            .prop_map(|(ip, port)| SocketAddr::V4(SocketAddrV4::new(ip.into(), port))),
        (any::<[u8; 16]>(), any::<u16>(), any::<u32>(), any::<u32>()).prop_map(
            |(ip, port, flow, scope)| SocketAddr::V6(SocketAddrV6::new(
                ip.into(),
                port,
                flow,
                scope
            ))
        ),
        Just(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::LOCALHOST,
            41000
        ))),
        Just(SocketAddr::V6(SocketAddrV6::new(
            Ipv4Addr::new(10, 0, 0, 1).to_ipv6_mapped(),
            41000,
            0,
            0
        ))),
    ];
    (kind, addr).prop_map(|(kind, addr)| Candidate { kind, addr })
}

fn mapping() -> impl Strategy<Value = Mapping> {
    prop_oneof![
        Just(Mapping::Easy),
        Just(Mapping::Hard),
        Just(Mapping::Unknown)
    ]
}

fn hostname() -> impl Strategy<Value = String> {
    let label = "[a-zA-Z0-9]([a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?";
    vec(label, 1..6)
        .prop_map(|labels| labels.join("."))
        .prop_filter("at most 253 characters", |name| name.len() <= 253)
        .prop_filter("a name, not an address", |name| {
            let last = name.rsplit('.').next().unwrap_or_default();
            let hex = last
                .strip_prefix("0x")
                .or_else(|| last.strip_prefix("0X"))
                .is_some_and(|rest| rest.bytes().all(|b| b.is_ascii_hexdigit()));
            let digits = last.bytes().all(|b| b.is_ascii_digit());
            !(hex || digits || last.eq_ignore_ascii_case("localhost"))
        })
}

fn usable_invite() -> impl Strategy<Value = Invite> {
    (
        (any::<[u8; 32]>(), any::<[u8; 8]>(), any::<[u8; 16]>()),
        (any::<bool>(), 0..=u64::from(u32::MAX)),
        vec(usable_candidate(), 0..=16),
        (mapping(), any::<bool>(), any::<bool>(), any::<bool>()),
        option::of(hostname()),
    )
        .prop_map(
            |(
                (host_key, invite_id, secret),
                (multi_use, expires_at),
                candidates,
                (mapping, mapped, mapped_verified, second_router),
                hostname,
            )| Invite {
                host_key,
                invite_id,
                secret,
                multi_use,
                expires_at,
                candidates,
                mapping,
                mapped,
                mapped_verified: mapped && mapped_verified,
                second_router,
                hostname,
            },
        )
}

fn usable_reply() -> impl Strategy<Value = ReplyCode> {
    (
        prop_oneof![
            any::<[u8; 8]>().prop_map(Answers::Invite),
            Just(Answers::Rejoin)
        ],
        any::<[u8; 32]>(),
        option::of(internet_v4()),
        option::of(internet_v6()),
        mapping(),
        0..=u64::from(u32::MAX),
    )
        .prop_map(
            |(answers, client_key, outside_v4, outside_v6, mapping, expires_at)| ReplyCode {
                answers,
                client_key,
                outside_v4,
                outside_v6,
                mapping,
                expires_at,
            },
        )
}

fn prefix() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        Just("booth1-".to_string()),
        Just("booth1-r-".to_string()),
        Just("BOOTH1-R-".to_string()),
        Just("\"booth1-".to_string()),
        "[bB]ooth[0-9]{0,30}-(r-)?",
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    #[test]
    fn invite_round_trip(invite in usable_invite()) {
        prop_assert!(invite.check().is_ok());
        let text = invite.encode();
        prop_assert_eq!(Invite::decode(&text), Ok(invite.clone()));
        prop_assert_eq!(Invite::decode(&text.to_uppercase()), Ok(invite));
    }

    #[test]
    fn reply_round_trip(code in usable_reply()) {
        prop_assert!(code.check().is_ok());
        let text = code.encode();
        prop_assert_eq!(ReplyCode::decode(&text), Ok(code));
        prop_assert_eq!(ReplyCode::decode(&text.to_uppercase()), Ok(code));
    }

    // Whatever new() accepts, decode must accept too. Mostly usable inputs, so most cases get
    // as far as decode.
    #[test]
    fn new_and_decode_agree(
        candidates in vec(prop_oneof![4 => usable_candidate(), 1 => any_candidate()], 0..18),
        hostname in option::of(prop_oneof![4 => hostname(), 1 => ".{0,300}"]),
        multi_use in any::<bool>(),
        now in prop_oneof![4 => 0..=u64::from(u32::MAX), 1 => any::<u64>()],
    ) {
        if let Ok(invite) = Invite::new(
            [1; 32], candidates, Mapping::Easy, true, false, false, hostname, multi_use, now,
        ) {
            prop_assert_eq!(Invite::decode(&invite.encode()), Ok(invite));
        }
    }

    #[test]
    fn decode_survives_any_string(text in any::<String>()) {
        let _ = Invite::decode(&text);
        let _ = ReplyCode::decode(&text);
    }

    #[test]
    fn decode_survives_code_shaped_text(
        prefix in prefix(),
        body in "[a-zA-Z2-7 \r\n\t\"'<>.,;=0-9\u{200b}\u{e9}-]{0,1200}",
    ) {
        let text = format!("{prefix}{body}");
        let _ = Invite::decode(&text);
        let _ = ReplyCode::decode(&text);
    }
}
