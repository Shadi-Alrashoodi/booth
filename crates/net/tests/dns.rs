use std::cell::RefCell;
use std::collections::HashSet;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use net::dns::{
    DnsError, Found, Kind, MAX_CHAIN, MAX_SERVERS, NameError, Nameservers, ParseError, Query,
    Rcode, Refused, Resolved, Resolver, Response, Source, System, SystemError, new_id,
};
use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;

const NAME: &str = "myroom.example.net";
const ID: u16 = 0x1234;
const HOME: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 5);
const HOME_V6: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 5);
const WAIT: Duration = Duration::from_millis(300);

fn ip(text: &str) -> IpAddr {
    text.parse().unwrap()
}

fn wire(name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for label in name.split('.').filter(|label| !label.is_empty()) {
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    out
}

fn question(name: &str, kind: u16) -> Vec<u8> {
    let mut out = wire(name);
    out.extend_from_slice(&kind.to_be_bytes());
    out.extend_from_slice(&[0, 1]);
    out
}

// Records as a nameserver lays them out. An owner that is the question's name
// is written as a pointer to it, the way every server compresses; any other
// name is written out whole.
#[derive(Clone)]
struct Rec {
    owner: String,
    kind: u16,
    class: u16,
    data: Vec<u8>,
}

fn a(owner: &str, ip: Ipv4Addr) -> Rec {
    raw(owner, 1, 1, &ip.octets())
}

fn aaaa(owner: &str, ip: Ipv6Addr) -> Rec {
    raw(owner, 28, 1, &ip.octets())
}

fn cname(owner: &str, target: &str) -> Rec {
    raw(owner, 5, 1, &wire(target))
}

fn ns(owner: &str, target: &str) -> Rec {
    raw(owner, 2, 1, &wire(target))
}

fn soa(owner: &str) -> Rec {
    let mut data = wire("ns1.example.net");
    data.extend(wire("hostmaster.example.net"));
    data.extend_from_slice(&[0; 20]);
    raw(owner, 6, 1, &data)
}

fn raw(owner: &str, kind: u16, class: u16, data: &[u8]) -> Rec {
    Rec {
        owner: owner.to_string(),
        kind,
        class,
        data: data.to_vec(),
    }
}

#[derive(Clone, Default)]
struct Reply {
    aa: bool,
    tc: bool,
    rcode: u8,
    answers: Vec<Rec>,
    authority: Vec<Rec>,
    additional: Vec<Rec>,
}

fn build(id: u16, question: &[u8], qname: &str, reply: &Reply) -> Vec<u8> {
    let mut flags: u16 = 0x8000 | u16::from(reply.rcode);
    if reply.aa {
        flags |= 0x0400;
    }
    if reply.tc {
        flags |= 0x0200;
    }
    let mut msg = Vec::new();
    msg.extend_from_slice(&id.to_be_bytes());
    msg.extend_from_slice(&flags.to_be_bytes());
    let counts = [
        1,
        reply.answers.len(),
        reply.authority.len(),
        reply.additional.len(),
    ];
    for count in counts {
        msg.extend_from_slice(&(count as u16).to_be_bytes());
    }
    msg.extend_from_slice(question);
    let records = reply
        .answers
        .iter()
        .chain(&reply.authority)
        .chain(&reply.additional);
    for rec in records {
        if !qname.is_empty() && rec.owner.eq_ignore_ascii_case(qname) {
            msg.extend_from_slice(&[0xc0, 0x0c]);
        } else {
            msg.extend_from_slice(&wire(&rec.owner));
        }
        msg.extend_from_slice(&rec.kind.to_be_bytes());
        msg.extend_from_slice(&rec.class.to_be_bytes());
        msg.extend_from_slice(&60u32.to_be_bytes());
        msg.extend_from_slice(&(rec.data.len() as u16).to_be_bytes());
        msg.extend_from_slice(&rec.data);
    }
    msg
}

fn query(name: &str, kind: Kind) -> Query {
    Query::new(name, kind, false, ID).unwrap()
}

// The answer to `query(name, kind)`.
fn answer(name: &str, kind: Kind, reply: &Reply) -> Vec<u8> {
    build(ID, &question(name, kind.code()), name, reply)
}

fn parse(name: &str, kind: Kind, msg: &[u8]) -> Result<Response, ParseError> {
    query(name, kind).parse(msg)
}

// A real-world answer from a dynamic DNS provider's own nameserver: the
// address, the zone's NS in authority and its glue in additional, every name
// but the first compressed.
#[rustfmt::skip]
fn captured() -> Vec<u8> {
    [
        // Id, then flags: answer, authoritative, NOERROR.
        &[0x12, 0x34, 0x84, 0x00][..],
        // One question, one answer, one authority and one additional record.
        &[0, 1, 0, 1, 0, 1, 0, 1],
        // The question, myroom.example.net A IN, at offset 12.
        &wire(NAME), &[0, 1, 0, 1],
        // The answer at 36: a pointer to the question's name, A, IN, 60 s.
        &[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 203, 0, 113, 5],
        // Authority at 52: example.net, a pointer into the question's name at
        // 19, NS ns1 plus a pointer back to example.net. The data is at 64.
        &[0xc0, 0x13, 0, 2, 0, 1, 0, 0, 0x0e, 0x10, 0, 6, 3, b'n', b's', b'1', 0xc0, 0x13],
        // Additional at 70: glue for ns1.example.net, a pointer to 64.
        &[0xc0, 0x40, 0, 1, 0, 1, 0, 0, 0x0e, 0x10, 0, 4, 99, 79, 143, 35],
    ]
    .concat()
}

#[test]
fn query_matches_the_rfc_layout() {
    let query = Query::new(NAME, Kind::A, false, 0xbeef).unwrap();
    #[rustfmt::skip]
    let want = [
        // Id, then flags with recursion desired clear.
        0xbe, 0xef, 0x00, 0x00,
        // One question, no answer, authority or additional records.
        0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        6, b'm', b'y', b'r', b'o', b'o', b'm',
        7, b'e', b'x', b'a', b'm', b'p', b'l', b'e',
        3, b'n', b'e', b't',
        0,
        // Type A, class IN.
        0x00, 0x01, 0x00, 0x01,
    ];
    assert_eq!(query.encode(), want);
}

#[test]
fn queries_carry_their_type_and_recursion_bit() {
    let aaaa = Query::new("MyRoom.Example.net.", Kind::Aaaa, true, 1)
        .unwrap()
        .encode();
    assert_eq!(aaaa[2..4], [0x01, 0x00]);
    assert_eq!(aaaa[aaaa.len() - 4..], [0, 28, 0, 1]);
    // The trailing dot goes, the case stays as given.
    assert_eq!(aaaa[12..aaaa.len() - 4], wire("MyRoom.Example.net")[..]);

    let ns = Query::new("example.net", Kind::Ns, false, 1)
        .unwrap()
        .encode();
    assert_eq!(ns[2..4], [0, 0]);
    assert_eq!(ns[ns.len() - 4..], [0, 2, 0, 1]);
}

#[test]
fn names_that_cannot_be_asked_are_refused() {
    let refused = |name: &str| Query::new(name, Kind::A, false, 1).err();
    assert_eq!(refused(""), Some(NameError::Empty));
    assert_eq!(refused("."), Some(NameError::Empty));
    assert_eq!(refused("a..org"), Some(NameError::EmptyLabel));
    assert_eq!(refused(".example.net"), Some(NameError::EmptyLabel));
    assert_eq!(
        refused(&format!("{}.org", "a".repeat(64))),
        Some(NameError::LongLabel(64))
    );
    assert_eq!(refused("my room.org"), Some(NameError::Char(' ')));
    assert_eq!(refused("münchen.de"), Some(NameError::Char('ü')));

    let label = "a".repeat(63);
    let longest = format!("{label}.{label}.{label}.{}", "b".repeat(61));
    assert_eq!(longest.len(), 253);
    assert_eq!(refused(&longest), None);
    let over = format!("{label}.{label}.{label}.{label}");
    assert_eq!(refused(&over), Some(NameError::Long(255)));
    assert_eq!(refused("_dns.my-room.example.net"), None);
}

#[test]
fn ids_are_random() {
    let ids: HashSet<u16> = (0..16).map(|_| new_id()).collect();
    assert!(ids.len() > 1);
}

#[test]
fn captured_answer() {
    let response = parse(NAME, Kind::A, &captured()).unwrap();
    assert!(response.authoritative);
    assert!(!response.truncated);
    assert_eq!(response.rcode, Rcode::NOERROR);
    assert_eq!(response.answer_count, 1);
    assert_eq!(response.addrs, [IpAddr::V4(HOME)]);
    assert!(response.nameservers.is_empty());
    assert_eq!(response.alias, None);
}

#[test]
fn answer_without_aa_flag() {
    let mut msg = captured();
    // Answer, recursion desired and available, no AA: what a resolver sends.
    msg[2..4].copy_from_slice(&[0x81, 0x80]);
    let response = parse(NAME, Kind::A, &msg).unwrap();
    assert!(!response.authoritative);
    assert_eq!(response.addrs, [IpAddr::V4(HOME)]);
}

#[test]
fn question_case_is_ignored() {
    let reply = Reply {
        aa: true,
        answers: vec![a(NAME, HOME)],
        ..Reply::default()
    };
    let msg = build(ID, &question("MyRoom.EXAMPLE.net", 1), NAME, &reply);
    assert_eq!(
        parse(NAME, Kind::A, &msg).unwrap().addrs,
        [IpAddr::V4(HOME)]
    );
}

#[test]
fn cname_chain_is_followed() {
    let reply = Reply {
        aa: true,
        // Out of order on purpose: the chain is followed by name, not place.
        answers: vec![
            a(NAME, HOME),
            cname("a.example.net", NAME),
            cname("room.example.net", "a.example.net"),
        ],
        ..Reply::default()
    };
    let msg = answer("room.example.net", Kind::A, &reply);
    let response = parse("room.example.net", Kind::A, &msg).unwrap();
    assert_eq!(response.addrs, [IpAddr::V4(HOME)]);
    assert_eq!(response.alias, None);
}

#[test]
fn cname_leaving_the_answer() {
    let reply = Reply {
        aa: true,
        answers: vec![cname("room.example.net", "MyRoom.Example.net")],
        ..Reply::default()
    };
    let msg = answer("room.example.net", Kind::A, &reply);
    let response = parse("room.example.net", Kind::A, &msg).unwrap();
    assert!(response.addrs.is_empty());
    assert_eq!(response.alias.as_deref(), Some(NAME));
}

fn chain(steps: usize) -> Vec<u8> {
    let mut answers: Vec<Rec> = (0..steps)
        .map(|i| {
            cname(
                &format!("c{i}.example.net"),
                &format!("c{}.example.net", i + 1),
            )
        })
        .collect();
    answers.push(a(&format!("c{steps}.example.net"), HOME));
    let reply = Reply {
        aa: true,
        answers,
        ..Reply::default()
    };
    answer("c0.example.net", Kind::A, &reply)
}

#[test]
fn chain_limit() {
    let followed = parse("c0.example.net", Kind::A, &chain(MAX_CHAIN)).unwrap();
    assert_eq!(followed.addrs, [IpAddr::V4(HOME)]);
    assert_eq!(
        parse("c0.example.net", Kind::A, &chain(MAX_CHAIN + 1)),
        Err(ParseError::LongChain)
    );
}

#[test]
fn cname_loop_is_refused() {
    let reply = Reply {
        aa: true,
        answers: vec![
            cname("a.example.net", "b.example.net"),
            cname("b.example.net", "a.example.net"),
        ],
        ..Reply::default()
    };
    let msg = answer("a.example.net", Kind::A, &reply);
    assert_eq!(
        parse("a.example.net", Kind::A, &msg),
        Err(ParseError::LongChain)
    );
}

#[test]
fn truncated_answer_is_marked() {
    let reply = Reply {
        aa: true,
        tc: true,
        answers: vec![a(NAME, HOME)],
        ..Reply::default()
    };
    let response = parse(NAME, Kind::A, &answer(NAME, Kind::A, &reply)).unwrap();
    assert!(response.truncated);
    assert!(response.addrs.is_empty());
    // Whatever follows the question in a cut answer is not even read.
    let mut cut = answer(NAME, Kind::A, &reply);
    cut.truncate(cut.len() - 3);
    assert!(parse(NAME, Kind::A, &cut).unwrap().truncated);
}

#[test]
fn nxdomain_and_servfail_come_back_as_rcodes() {
    let missing = Reply {
        aa: true,
        rcode: 3,
        authority: vec![soa("example.net")],
        ..Reply::default()
    };
    let response = parse(NAME, Kind::A, &answer(NAME, Kind::A, &missing)).unwrap();
    assert_eq!(response.rcode, Rcode::NXDOMAIN);
    assert!(response.authoritative);
    assert!(response.addrs.is_empty());

    let failed = Reply {
        rcode: 2,
        ..Reply::default()
    };
    let response = parse(NAME, Kind::A, &answer(NAME, Kind::A, &failed)).unwrap();
    assert_eq!(response.rcode, Rcode::SERVFAIL);
    assert_eq!(Rcode::SERVFAIL.to_string(), "SERVFAIL (2)");
}

#[test]
fn ns_answers_give_the_nameserver_names() {
    let reply = Reply {
        aa: true,
        answers: vec![
            ns("example.net", "ns1.example.net"),
            ns("example.net", "NS2.Example.net"),
        ],
        ..Reply::default()
    };
    let response = parse(
        "example.net",
        Kind::Ns,
        &answer("example.net", Kind::Ns, &reply),
    )
    .unwrap();
    assert_eq!(response.nameservers, ["ns1.example.net", "ns2.example.net"]);
    assert!(response.addrs.is_empty());
}

#[test]
fn aaaa_answers_give_only_ipv6_addresses() {
    let reply = Reply {
        aa: true,
        answers: vec![a(NAME, HOME), aaaa(NAME, HOME_V6)],
        ..Reply::default()
    };
    let response = parse(NAME, Kind::Aaaa, &answer(NAME, Kind::Aaaa, &reply)).unwrap();
    assert_eq!(response.addrs, [IpAddr::V6(HOME_V6)]);
}

#[test]
fn other_records_are_left_out() {
    let reply = Reply {
        aa: true,
        answers: vec![
            a("other.example.net", Ipv4Addr::new(198, 51, 100, 1)),
            raw(NAME, 16, 1, b"\x05hello"),
            raw(NAME, 1, 3, &[198, 51, 100, 2]),
            a(NAME, HOME),
        ],
        // An EDNS record, whose class field is a size, owned by the root.
        additional: vec![raw("", 41, 1232, &[])],
        ..Reply::default()
    };
    let response = parse(NAME, Kind::A, &answer(NAME, Kind::A, &reply)).unwrap();
    assert_eq!(response.addrs, [IpAddr::V4(HOME)]);
    assert_eq!(response.answer_count, 4);
}

#[test]
fn answer_to_another_query() {
    let good = captured();

    let mut wrong_id = good.clone();
    wrong_id[1] ^= 1;
    assert_eq!(parse(NAME, Kind::A, &wrong_id), Err(ParseError::Id(ID ^ 1)));

    let reply = Reply {
        aa: true,
        answers: vec![a("other.example.net", HOME)],
        ..Reply::default()
    };
    let other_name = build(ID, &question("other.example.net", 1), "", &reply);
    assert_eq!(parse(NAME, Kind::A, &other_name), Err(ParseError::Question));
    assert_eq!(parse(NAME, Kind::Aaaa, &good), Err(ParseError::Question));
    let mut chaos = good.clone();
    chaos[35] = 3;
    assert_eq!(parse(NAME, Kind::A, &chaos), Err(ParseError::Question));

    let mut none = good.clone();
    none[5] = 0;
    assert_eq!(parse(NAME, Kind::A, &none), Err(ParseError::Questions(0)));
    let mut two = good.clone();
    two[5] = 2;
    assert_eq!(parse(NAME, Kind::A, &two), Err(ParseError::Questions(2)));
}

#[test]
fn query_or_other_opcode() {
    let mut asked = captured();
    asked[2] &= 0x7f;
    assert_eq!(parse(NAME, Kind::A, &asked), Err(ParseError::NotAnswer));
    let mut status = captured();
    status[2] |= 0x10;
    assert_eq!(parse(NAME, Kind::A, &status), Err(ParseError::Opcode(2)));
}

#[test]
fn short_messages_are_refused() {
    let good = captured();
    assert_eq!(
        parse(NAME, Kind::A, &good[..11]),
        Err(ParseError::Short(11))
    );
    assert_eq!(parse(NAME, Kind::A, &good[..12]), Err(ParseError::PastEnd));
    assert_eq!(parse(NAME, Kind::A, &good[..34]), Err(ParseError::PastEnd));
}

// The captured answer with its answer record's owner (at 36, two bytes of
// pointer) replaced by `owner`.
fn with_owner(owner: &[u8]) -> Vec<u8> {
    let good = captured();
    [&good[..36], owner, &good[38..]].concat()
}

#[test]
fn pointers_that_do_not_point_back_are_refused() {
    // To itself.
    assert_eq!(
        parse(NAME, Kind::A, &with_owner(&[0xc0, 36])),
        Err(ParseError::Pointer(36))
    );
    // A label, then back to its own start: a loop without the pointer ever
    // pointing forward.
    assert_eq!(
        parse(NAME, Kind::A, &with_owner(&[1, b'a', 0xc0, 36])),
        Err(ParseError::Pointer(36))
    );
    // Forward, where a second name would point back at this one.
    assert_eq!(
        parse(NAME, Kind::A, &with_owner(&[0xc0, 52])),
        Err(ParseError::Pointer(52))
    );
    // Past the end, and into the header.
    assert_eq!(
        parse(NAME, Kind::A, &with_owner(&[0xff, 0xff])),
        Err(ParseError::Pointer(0x3fff))
    );
    assert_eq!(
        parse(NAME, Kind::A, &with_owner(&[0xc0, 5])),
        Err(ParseError::Pointer(5))
    );
    // In the question, where there is nothing earlier to point at.
    let mut msg = captured();
    msg[12..14].copy_from_slice(&[0xc0, 12]);
    assert_eq!(parse(NAME, Kind::A, &msg), Err(ParseError::Pointer(12)));
}

#[test]
fn unknown_label_types() {
    assert_eq!(
        parse(NAME, Kind::A, &with_owner(&[0x41, 0])),
        Err(ParseError::Label(0x41))
    );
    assert_eq!(
        parse(NAME, Kind::A, &with_owner(&[0x80, 0])),
        Err(ParseError::Label(0x80))
    );
}

#[test]
fn names_over_255_bytes_are_refused() {
    let label = "a".repeat(63);
    let long = wire(&format!("{label}.{label}.{label}.{label}"));
    assert_eq!(
        parse(NAME, Kind::A, &with_owner(&long)),
        Err(ParseError::LongName)
    );

    // Short where it stands, too long once its pointer is followed: a second
    // answer of 64 bytes pointing at the first answer's 193-byte owner.
    let good = captured();
    let three = wire(&format!("{label}.{label}.{label}"));
    let mut second = vec![63];
    second.extend_from_slice(label.as_bytes());
    second.extend_from_slice(&[0xc0, 36, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 1, 2, 3, 4]);
    let mut msg = [&good[..36], &three[..], &good[38..52], &second[..]].concat();
    msg[7] = 2;
    assert_eq!(parse(NAME, Kind::A, &msg), Err(ParseError::LongName));
}

#[test]
fn counts_that_run_past_the_end_are_refused() {
    for at in [6, 8, 10] {
        let mut msg = captured();
        msg[at..at + 2].copy_from_slice(&[0xff, 0xff]);
        assert_eq!(
            parse(NAME, Kind::A, &msg),
            Err(ParseError::PastEnd),
            "count at {at}"
        );
    }
    // A record whose data length runs past the end.
    let mut msg = captured();
    msg[46..48].copy_from_slice(&[0x01, 0x00]);
    assert_eq!(parse(NAME, Kind::A, &msg), Err(ParseError::PastEnd));
}

#[test]
fn records_of_the_wrong_size_are_refused() {
    let reply = Reply {
        aa: true,
        answers: vec![raw(NAME, 1, 1, &[203, 0, 113, 5, 0])],
        ..Reply::default()
    };
    assert_eq!(
        parse(NAME, Kind::A, &answer(NAME, Kind::A, &reply)),
        Err(ParseError::Record { kind: 1, len: 5 })
    );

    let mut data = wire("ns1.example.net");
    data.push(0);
    let reply = Reply {
        aa: true,
        answers: vec![raw("example.net", 2, 1, &data)],
        ..Reply::default()
    };
    assert_eq!(
        parse(
            "example.net",
            Kind::Ns,
            &answer("example.net", Kind::Ns, &reply)
        ),
        Err(ParseError::Record { kind: 2, len: 18 })
    );
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 4096,
        failure_persistence: Some(Box::new(FileFailurePersistence::WithSource("regressions"))),
        ..ProptestConfig::default()
    })]

    #[test]
    fn random_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..700)) {
        for kind in [Kind::A, Kind::Aaaa, Kind::Ns] {
            let _ = parse(NAME, kind, &bytes);
        }
    }

    // Noise almost never gets past the id and the question, so this keeps
    // both and makes up the flags, the counts and everything after.
    #[test]
    fn made_up_records_never_give_the_wrong_family(
        flags in any::<u16>(),
        counts in proptest::array::uniform3(0u16..8),
        body in proptest::collection::vec(any::<u8>(), 0..400),
    ) {
        for (kind, v4) in [(Kind::A, true), (Kind::Aaaa, false)] {
            let mut msg = ID.to_be_bytes().to_vec();
            msg.extend_from_slice(&((flags | 0x8000) & !0x7800).to_be_bytes());
            msg.extend_from_slice(&[0, 1]);
            for count in counts {
                msg.extend_from_slice(&count.to_be_bytes());
            }
            msg.extend_from_slice(&question(NAME, kind.code()));
            msg.extend_from_slice(&body);
            if let Ok(response) = parse(NAME, kind, &msg) {
                for ip in &response.addrs {
                    prop_assert_eq!(ip.is_ipv4(), v4);
                }
            }
        }
    }

    // A real answer with a few bytes changed keeps most of its framing, which
    // takes the parser deepest.
    #[test]
    fn damaged_answers_never_panic(
        changes in proptest::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..6),
    ) {
        let mut msg = captured();
        for (at, byte) in changes {
            let at = at.index(msg.len());
            msg[at] = byte;
        }
        let _ = parse(NAME, Kind::A, &msg);
    }
}

// The system resolver, from a table. Every call is recorded as "NS name" or
// "A name", with " fresh" when the cache was to be bypassed.
#[derive(Default)]
struct FakeSystem {
    ns: Vec<(String, Result<Vec<String>, SystemError>)>,
    addrs: Vec<(String, Kind, Result<Vec<IpAddr>, SystemError>)>,
    calls: RefCell<Vec<String>>,
}

impl FakeSystem {
    fn ns(mut self, name: &str, names: &[&str]) -> FakeSystem {
        let names = names.iter().map(ToString::to_string).collect();
        self.ns.push((name.to_string(), Ok(names)));
        self
    }

    fn ns_fails(mut self, name: &str, err: SystemError) -> FakeSystem {
        self.ns.push((name.to_string(), Err(err)));
        self
    }

    fn addrs(mut self, name: &str, kind: Kind, ips: &[&str]) -> FakeSystem {
        let ips = ips.iter().map(|&text| ip(text)).collect();
        self.addrs.push((name.to_string(), kind, Ok(ips)));
        self
    }

    fn addrs_fail(mut self, name: &str, kind: Kind, err: SystemError) -> FakeSystem {
        self.addrs.push((name.to_string(), kind, Err(err)));
        self
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

impl System for FakeSystem {
    fn nameservers(&self, name: &str) -> Result<Vec<String>, SystemError> {
        self.calls.borrow_mut().push(format!("NS {name}"));
        self.ns
            .iter()
            .find(|(n, _)| n == name)
            .map_or(Ok(Vec::new()), |(_, found)| found.clone())
    }

    fn addresses(&self, name: &str, kind: Kind, fresh: bool) -> Result<Vec<IpAddr>, SystemError> {
        let fresh = if fresh { " fresh" } else { "" };
        self.calls
            .borrow_mut()
            .push(format!("{kind} {name}{fresh}"));
        self.addrs
            .iter()
            .find(|(n, k, _)| n == name && *k == kind)
            .map_or(Ok(Vec::new()), |(_, _, found)| found.clone())
    }
}

// Lets loopback through, so the fakes can be reached.
fn loopback_ok(addr: SocketAddr) -> Result<(), &'static str> {
    if addr.port() == 0 {
        Err("the port is 0")
    } else if addr.ip().is_unspecified() {
        Err("it is the unspecified address")
    } else if addr.ip().is_multicast() {
        Err("it is a multicast address")
    } else {
        Ok(())
    }
}

// The rules of invite::check_addr, which these tests cannot reach.
fn like_invite(addr: SocketAddr) -> Result<(), &'static str> {
    if addr.ip().is_loopback() {
        return Err("it is a loopback address");
    }
    loopback_ok(addr)
}

fn resolver<'a>(
    system: &'a FakeSystem,
    check: &'a dyn Fn(SocketAddr) -> Result<(), &'static str>,
) -> Resolver<'a> {
    Resolver {
        system,
        check,
        port: 53,
        timeout: WAIT,
    }
}

fn servers(addrs: &[SocketAddr]) -> Nameservers {
    Nameservers {
        zone: "example.net".to_string(),
        names: Vec::new(),
        addrs: addrs.to_vec(),
    }
}

fn noted<T>(run: impl FnOnce(&mut dyn FnMut(fmt::Arguments<'_>)) -> T) -> (T, Vec<String>) {
    let mut lines = Vec::new();
    let out = run(&mut |args: fmt::Arguments<'_>| lines.push(args.to_string()));
    (out, lines)
}

fn has(lines: &[String], text: &str) -> bool {
    lines.iter().any(|line| line.contains(text))
}

fn found(ip: IpAddr, source: Source) -> Found {
    Found { ip, source }
}

#[derive(Clone, Debug)]
struct Asked {
    id: u16,
    recursion: bool,
    name: String,
    kind: u16,
    question: Vec<u8>,
    from: SocketAddr,
}

fn decode_query(query: &[u8], from: SocketAddr) -> Asked {
    let mut labels = Vec::new();
    let mut at = 12;
    while query[at] != 0 {
        let len = usize::from(query[at]);
        labels.push(String::from_utf8_lossy(&query[at + 1..at + 1 + len]).into_owned());
        at += 1 + len;
    }
    Asked {
        id: u16::from_be_bytes([query[0], query[1]]),
        recursion: query[2] & 0x01 != 0,
        name: labels.join("."),
        kind: u16::from_be_bytes([query[at + 1], query[at + 2]]),
        question: query[12..at + 5].to_vec(),
        from,
    }
}

enum Out {
    Send(Vec<u8>),
    // To an earlier asker, whose answer was held back.
    SendTo(SocketAddr, Vec<u8>),
    FromElsewhere(Vec<u8>),
    Wait(Duration),
}

fn reply_to(asked: &Asked, reply: &Reply) -> Out {
    Out::Send(build(asked.id, &asked.question, &asked.name, reply))
}

fn authoritative(answers: Vec<Rec>) -> Reply {
    Reply {
        aa: true,
        answers,
        ..Reply::default()
    }
}

// One test at a time with a stand-in nameserver, and with
// closed_port_falls_back_at_once. Windows hands out ports in order from one
// counter for the whole PC. Each of the resolver's query sockets, opened with
// SO_RANDOMIZE_PORT, moves that counter to just past its random port, and a
// random pick inside one of Windows' excluded port ranges gets the first free
// port after the range, so a few ports come up far more often than chance.
// The port that test has just let go of can then be bound again at once by
// another test's nameserver or query socket, which answers in its place or
// swallows the question. Fake::start asks for the turn so that no test can
// open one without it.
static TURN: Mutex<()> = Mutex::new(());

type Turn = MutexGuard<'static, ()>;

fn turn() -> Turn {
    TURN.lock().unwrap_or_else(PoisonError::into_inner)
}

// A nameserver on loopback that answers each query with whatever `answer`
// makes of it, one query at a time.
struct Fake<'a> {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Asked>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    _turn: &'a Turn,
}

impl<'a> Fake<'a> {
    fn start(turn: &'a Turn, answer: impl Fn(&Asked) -> Vec<Out> + Send + 'static) -> Fake<'a> {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let elsewhere = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(10)))
            .unwrap();
        let addr = socket.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<Asked>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let seen = Arc::clone(&seen);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let mut buf = [0u8; 512];
                while !stop.load(Ordering::Relaxed) {
                    // Timeouts, and the port unreachable Windows reports
                    // after answering a socket that has since closed.
                    let Ok((len, from)) = socket.recv_from(&mut buf) else {
                        continue;
                    };
                    let asked = decode_query(&buf[..len], from);
                    let outs = answer(&asked);
                    seen.lock().unwrap().push(asked);
                    for out in outs {
                        match out {
                            Out::Send(msg) => {
                                let _ = socket.send_to(&msg, from);
                            }
                            Out::SendTo(to, msg) => {
                                let _ = socket.send_to(&msg, to);
                            }
                            Out::FromElsewhere(msg) => {
                                let _ = elsewhere.send_to(&msg, from);
                            }
                            Out::Wait(wait) => thread::sleep(wait),
                        }
                    }
                }
            })
        };
        Fake {
            addr,
            seen,
            stop,
            thread: Some(thread),
            _turn: turn,
        }
    }

    fn seen(&self) -> Vec<Asked> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for Fake<'_> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// Answers A with `v4` and AAAA with `v6`, authoritatively; an empty list is
// the name without a record of that type.
fn home_server(turn: &Turn, v4: Vec<Ipv4Addr>, v6: Vec<Ipv6Addr>) -> Fake<'_> {
    Fake::start(turn, move |asked| {
        let records = if asked.kind == 1 {
            v4.iter().map(|ip| a(&asked.name, *ip)).collect()
        } else {
            v6.iter().map(|ip| aaaa(&asked.name, *ip)).collect()
        };
        vec![reply_to(asked, &authoritative(records))]
    })
}

#[test]
fn walk_up_to_nameservers() {
    let system = FakeSystem::default()
        .ns("example.net", &["ns1.example.net", "NS2.example.net."])
        .addrs("ns1.example.net", Kind::A, &["127.0.0.11"])
        .addrs("ns2.example.net", Kind::A, &["127.0.0.12"]);
    let r = resolver(&system, &loopback_ok);
    let (walked, lines) = noted(|note| r.nameservers("MyRoom.Example.net.", note));
    assert_eq!(
        walked,
        Ok(Nameservers {
            zone: "example.net".to_string(),
            names: vec!["ns1.example.net".to_string(), "ns2.example.net".to_string()],
            addrs: vec![
                "127.0.0.11:53".parse().unwrap(),
                "127.0.0.12:53".parse().unwrap(),
            ],
        })
    );
    assert_eq!(
        system.calls(),
        [
            "NS myroom.example.net",
            "NS example.net",
            "A ns1.example.net",
            "AAAA ns1.example.net",
            "A ns2.example.net",
            "AAAA ns2.example.net",
        ]
    );
    assert!(has(
        &lines,
        "myroom.example.net has no nameservers of its own"
    ));
    assert!(has(
        &lines,
        "nameserver ns1.example.net is at 127.0.0.11:53"
    ));
}

#[test]
fn walk_past_errors() {
    let system = FakeSystem::default()
        .ns_fails(NAME, SystemError::NoSuchName)
        .ns_fails(
            "example.net",
            SystemError::Failed("the system resolver timed out".to_string()),
        )
        .ns("net", &["a.gtld-servers.net"])
        .addrs("a.gtld-servers.net", Kind::A, &["127.0.0.20"]);
    let r = resolver(&system, &loopback_ok);
    let (walked, lines) = noted(|note| r.nameservers(NAME, note));
    assert_eq!(walked.unwrap().zone, "net");
    assert!(has(&lines, "example.net: the system resolver timed out"));

    let nothing = FakeSystem::default();
    let r = resolver(&nothing, &loopback_ok);
    let (walked, _) = noted(|note| r.nameservers(NAME, note));
    assert_eq!(walked, Err(DnsError::NoNameservers(NAME.to_string())));
    assert_eq!(
        nothing.calls(),
        ["NS myroom.example.net", "NS example.net", "NS net"]
    );
}

#[test]
fn walk_takes_four_servers_at_most() {
    let system = FakeSystem::default()
        .ns(
            "example.net",
            &["ns1.x", "ns2.x", "ns3.x", "ns4.x", "ns5.x"],
        )
        .addrs("ns1.x", Kind::A, &["127.0.0.11"])
        .addrs("ns2.x", Kind::A, &["127.0.0.12"])
        .addrs("ns3.x", Kind::A, &["127.0.0.13"])
        .addrs("ns4.x", Kind::A, &["127.0.0.14"])
        .addrs("ns5.x", Kind::A, &["127.0.0.15"]);
    let r = resolver(&system, &loopback_ok);
    let (walked, _) = noted(|note| r.nameservers(NAME, note));
    let walked = walked.unwrap();
    assert_eq!(walked.addrs.len(), MAX_SERVERS);
    assert_eq!(walked.names.len(), 5);
    assert!(!system.calls().iter().any(|call| call.contains("ns5.x")));
}

#[test]
fn walk_takes_one_address_per_family() {
    let system = FakeSystem::default()
        .ns("example.net", &["ns1.x", "ns2.x"])
        .addrs("ns1.x", Kind::A, &["127.0.0.11", "127.0.0.21"])
        .addrs("ns1.x", Kind::Aaaa, &["::1"])
        .addrs_fail(
            "ns2.x",
            Kind::A,
            SystemError::Failed("the system resolver timed out".to_string()),
        )
        .addrs("ns2.x", Kind::Aaaa, &["::1"]);
    let r = resolver(&system, &loopback_ok);
    let (walked, lines) = noted(|note| r.nameservers(NAME, note));
    let mut want: Vec<SocketAddr> = vec!["127.0.0.11:53".parse().unwrap()];
    // Without IPv6 on this PC there is no route to ::1, and it is skipped.
    if UdpSocket::bind("[::1]:0").is_ok() {
        want.push("[::1]:53".parse().unwrap());
    }
    assert_eq!(walked.unwrap().addrs, want);
    assert!(has(
        &lines,
        "could not find the A address of nameserver ns2.x"
    ));
}

#[test]
fn walk_refuses_checked_addresses() {
    let system = FakeSystem::default()
        .ns("example.net", &["ns1.x", "ns2.x"])
        .addrs("ns1.x", Kind::A, &["127.0.0.11", "127.0.0.21"])
        .addrs("ns2.x", Kind::A, &["127.0.0.12"]);
    let check = |addr: SocketAddr| {
        if addr.ip() == ip("127.0.0.11") {
            Err("refused in this test")
        } else {
            loopback_ok(addr)
        }
    };
    let r = resolver(&system, &check);
    let (walked, lines) = noted(|note| r.nameservers(NAME, note));
    assert_eq!(
        walked.unwrap().addrs,
        [
            "127.0.0.21:53".parse::<SocketAddr>().unwrap(),
            "127.0.0.12:53".parse().unwrap(),
        ]
    );
    assert!(has(
        &lines,
        "refused nameserver ns1.x at 127.0.0.11:53: refused in this test"
    ));

    // With the rules invite::check_addr applies, loopback nameservers are
    // refused and the walk has nobody to ask.
    let r = resolver(&system, &like_invite);
    let (walked, _) = noted(|note| r.nameservers(NAME, note));
    assert_eq!(
        walked,
        Err(DnsError::NoServerAddress("example.net".to_string()))
    );
}

#[test]
fn bad_name_is_refused_first() {
    let system = FakeSystem::default();
    let r = resolver(&system, &loopback_ok);
    let (walked, _) = noted(|note| r.nameservers("my room.org", note));
    assert!(matches!(walked, Err(DnsError::Name { .. })));
    let (resolved, _) = noted(|note| r.resolve("a..b", None, note));
    assert!(matches!(resolved, Err(DnsError::Name { .. })));
    assert!(system.calls().is_empty());
}

#[test]
fn authoritative_answer() {
    let turn = turn();
    // The resolver closes each query socket as soon as its answer is in.
    // Answered at once, the first socket can be gone before the second one
    // opens, and Windows may then give the second the same port. Holding the
    // first answer until the second question is in keeps both open together,
    // so the ports checked below have to differ.
    let waiting: Mutex<Option<(SocketAddr, Vec<u8>)>> = Mutex::new(None);
    let fake = Fake::start(&turn, move |asked| {
        let record = if asked.kind == 1 {
            a(&asked.name, HOME)
        } else {
            aaaa(&asked.name, HOME_V6)
        };
        let msg = build(
            asked.id,
            &asked.question,
            &asked.name,
            &authoritative(vec![record]),
        );
        let mut slot = waiting.lock().unwrap();
        match slot.take() {
            Some((first, held)) => vec![Out::SendTo(first, held), Out::Send(msg)],
            None => {
                *slot = Some((asked.from, msg));
                Vec::new()
            }
        }
    });
    let system = FakeSystem::default();
    // The first query's wait now covers the second one being sent too, which
    // a busy PC can stretch past WAIT. A pass costs no more time for it.
    let r = Resolver {
        timeout: Duration::from_secs(2),
        ..resolver(&system, &loopback_ok)
    };
    let (resolved, lines) =
        noted(|note| r.resolve("MyRoom.Example.net", Some(&servers(&[fake.addr])), note));
    assert_eq!(
        resolved,
        Ok(Resolved {
            addrs: vec![
                found(IpAddr::V4(HOME), Source::Authoritative),
                found(IpAddr::V6(HOME_V6), Source::Authoritative),
            ],
            refused: Vec::new(),
        })
    );
    assert!(system.calls().is_empty());
    assert!(has(&lines, "took 203.0.113.5 (authoritative)"));

    // One question per query, each from its own socket, recursion off.
    let seen = fake.seen();
    assert_eq!(seen.len(), 2);
    assert!(
        seen.iter()
            .all(|asked| !asked.recursion && asked.name == NAME)
    );
    let kinds: HashSet<u16> = seen.iter().map(|asked| asked.kind).collect();
    assert_eq!(kinds, HashSet::from([1, 28]));
    assert_ne!(seen[0].from.port(), seen[1].from.port());
}

#[test]
fn answer_without_authority() {
    let turn = turn();
    let fake = Fake::start(&turn, |asked| {
        let records = if asked.kind == 1 {
            vec![a(&asked.name, HOME)]
        } else {
            Vec::new()
        };
        // A resolver's flags: recursion available, no AA.
        let reply = Reply {
            answers: records,
            authority: vec![soa("example.net")],
            ..Reply::default()
        };
        vec![reply_to(asked, &reply)]
    });
    let system = FakeSystem::default();
    let r = resolver(&system, &loopback_ok);
    let (resolved, lines) = noted(|note| r.resolve(NAME, Some(&servers(&[fake.addr])), note));
    assert_eq!(
        resolved.unwrap().addrs,
        [found(IpAddr::V4(HOME), Source::NotAuthoritative)]
    );
    assert!(has(
        &lines,
        "is not authoritative: something between this PC"
    ));
}

#[test]
fn truncated_answer_falls_back() {
    let turn = turn();
    let fake = Fake::start(&turn, |asked| {
        let reply = if asked.kind == 1 {
            Reply {
                aa: true,
                tc: true,
                answers: vec![a(&asked.name, Ipv4Addr::new(198, 51, 100, 7))],
                ..Reply::default()
            }
        } else {
            authoritative(Vec::new())
        };
        vec![reply_to(asked, &reply)]
    });
    let system = FakeSystem::default().addrs(NAME, Kind::A, &["203.0.113.5"]);
    let r = resolver(&system, &loopback_ok);
    let (resolved, lines) = noted(|note| r.resolve(NAME, Some(&servers(&[fake.addr])), note));
    assert_eq!(
        resolved.unwrap().addrs,
        [found(IpAddr::V4(HOME), Source::System)]
    );
    assert_eq!(system.calls(), ["A myroom.example.net fresh"]);
    assert!(has(&lines, "is truncated"));
}

#[test]
fn silent_nameserver_falls_back() {
    let turn = turn();
    let fake = Fake::start(&turn, |_| Vec::new());
    let system = FakeSystem::default().addrs(NAME, Kind::A, &["203.0.113.5"]);
    let r = resolver(&system, &loopback_ok);
    let started = Instant::now();
    let (resolved, lines) = noted(|note| r.resolve(NAME, Some(&servers(&[fake.addr])), note));
    assert!(started.elapsed() >= WAIT);
    assert_eq!(
        resolved.unwrap().addrs,
        [found(IpAddr::V4(HOME), Source::System)]
    );
    assert_eq!(
        system.calls(),
        [
            "A myroom.example.net fresh",
            "AAAA myroom.example.net fresh"
        ]
    );
    assert!(has(&lines, "no A answer from"));
}

#[test]
fn closed_port_falls_back_at_once() {
    let _turn = turn();
    let closed = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap();
    let system = FakeSystem::default().addrs(NAME, Kind::A, &["203.0.113.5"]);
    let r = Resolver {
        timeout: Duration::from_secs(5),
        ..resolver(&system, &loopback_ok)
    };
    let started = Instant::now();
    let (resolved, lines) = noted(|note| r.resolve(NAME, Some(&servers(&[closed])), note));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(
        resolved.unwrap().addrs,
        [found(IpAddr::V4(HOME), Source::System)]
    );
    assert!(has(&lines, "answered port unreachable"));
}

#[test]
fn wrong_ids_and_wrong_questions_are_waited_past() {
    let turn = turn();
    let fake = Fake::start(&turn, |asked| {
        if asked.kind != 1 {
            return vec![reply_to(asked, &authoritative(Vec::new()))];
        }
        let decoy = authoritative(vec![a(&asked.name, Ipv4Addr::new(198, 51, 100, 8))]);
        let other = question("other.example.net", 1);
        vec![
            Out::Send(build(asked.id ^ 1, &asked.question, &asked.name, &decoy)),
            Out::Send(build(asked.id, &other, "", &decoy)),
            reply_to(asked, &authoritative(vec![a(&asked.name, HOME)])),
        ]
    });
    let system = FakeSystem::default();
    let r = resolver(&system, &loopback_ok);
    let (resolved, lines) = noted(|note| r.resolve(NAME, Some(&servers(&[fake.addr])), note));
    assert_eq!(
        resolved.unwrap().addrs,
        [found(IpAddr::V4(HOME), Source::Authoritative)]
    );
    assert!(has(&lines, "is not the one asked"));
    assert!(has(&lines, "its question is not the one asked"));
}

#[test]
fn answer_from_another_source() {
    let turn = turn();
    let fake = Fake::start(&turn, |asked| {
        let forged = authoritative(vec![a(&asked.name, Ipv4Addr::new(198, 51, 100, 9))]);
        vec![Out::FromElsewhere(build(
            asked.id,
            &asked.question,
            &asked.name,
            &forged,
        ))]
    });
    let system = FakeSystem::default().addrs(NAME, Kind::A, &["203.0.113.5"]);
    let r = resolver(&system, &loopback_ok);
    let (resolved, _) = noted(|note| r.resolve(NAME, Some(&servers(&[fake.addr])), note));
    assert_eq!(
        resolved.unwrap().addrs,
        [found(IpAddr::V4(HOME), Source::System)]
    );
}

#[test]
fn servfail_waits_for_other_servers() {
    let turn = turn();
    let failing = Fake::start(&turn, |asked| {
        let reply = Reply {
            rcode: 2,
            ..Reply::default()
        };
        vec![reply_to(asked, &reply)]
    });
    let slow = Fake::start(&turn, |asked| {
        let records = if asked.kind == 1 {
            vec![a(&asked.name, HOME)]
        } else {
            Vec::new()
        };
        vec![
            Out::Wait(Duration::from_millis(50)),
            reply_to(asked, &authoritative(records)),
        ]
    });
    let system = FakeSystem::default();
    let r = resolver(&system, &loopback_ok);
    let (resolved, lines) =
        noted(|note| r.resolve(NAME, Some(&servers(&[failing.addr, slow.addr])), note));
    assert_eq!(
        resolved.unwrap().addrs,
        [found(IpAddr::V4(HOME), Source::Authoritative)]
    );
    assert!(has(&lines, "SERVFAIL (2)"));
    assert!(system.calls().is_empty());
}

#[test]
fn first_valid_answer_wins() {
    let turn = turn();
    let quick = home_server(&turn, vec![Ipv4Addr::new(198, 51, 100, 1)], Vec::new());
    let late = Fake::start(&turn, |asked| {
        let records = if asked.kind == 1 {
            vec![a(&asked.name, Ipv4Addr::new(198, 51, 100, 2))]
        } else {
            Vec::new()
        };
        vec![
            Out::Wait(Duration::from_millis(100)),
            reply_to(asked, &authoritative(records)),
        ]
    });
    let system = FakeSystem::default();
    let r = resolver(&system, &loopback_ok);
    let (resolved, _) =
        noted(|note| r.resolve(NAME, Some(&servers(&[late.addr, quick.addr])), note));
    assert_eq!(
        resolved.unwrap().addrs,
        [found(ip("198.51.100.1"), Source::Authoritative)]
    );
}

#[test]
fn authoritative_nxdomain() {
    let turn = turn();
    let fake = Fake::start(&turn, |asked| {
        let reply = Reply {
            aa: true,
            rcode: 3,
            authority: vec![soa("example.net")],
            ..Reply::default()
        };
        vec![reply_to(asked, &reply)]
    });
    let system = FakeSystem::default().addrs(NAME, Kind::A, &["203.0.113.5"]);
    let r = resolver(&system, &loopback_ok);
    let (resolved, lines) = noted(|note| r.resolve(NAME, Some(&servers(&[fake.addr])), note));
    assert_eq!(resolved, Err(DnsError::NoSuchName(NAME.to_string())));
    assert!(system.calls().is_empty());
    assert!(has(&lines, "does not exist (authoritative"));
}

// What a network that redirects port 53 to its own filtering resolver sends
// back for a dynamic DNS name. Windows' own resolver may go around it.
#[test]
fn nxdomain_without_authority() {
    let turn = turn();
    let fake = Fake::start(&turn, |asked| {
        let reply = Reply {
            rcode: 3,
            ..Reply::default()
        };
        vec![reply_to(asked, &reply)]
    });
    let system = FakeSystem::default().addrs(NAME, Kind::A, &["203.0.113.5"]);
    let r = resolver(&system, &loopback_ok);
    let started = Instant::now();
    let (resolved, lines) = noted(|note| r.resolve(NAME, Some(&servers(&[fake.addr])), note));
    assert!(started.elapsed() < WAIT, "nothing more to wait for");
    assert_eq!(
        resolved.unwrap().addrs,
        [found(IpAddr::V4(HOME), Source::System)]
    );
    assert_eq!(
        system.calls(),
        [
            "A myroom.example.net fresh",
            "AAAA myroom.example.net fresh"
        ]
    );
    assert!(has(&lines, "but not authoritatively"));

    // An authoritative answer from another nameserver still settles it.
    let home = Fake::start(&turn, |asked| {
        let reply = Reply {
            aa: true,
            rcode: 3,
            ..Reply::default()
        };
        vec![
            Out::Wait(Duration::from_millis(50)),
            reply_to(asked, &reply),
        ]
    });
    let system = FakeSystem::default().addrs(NAME, Kind::A, &["203.0.113.5"]);
    let r = resolver(&system, &loopback_ok);
    let (resolved, _) =
        noted(|note| r.resolve(NAME, Some(&servers(&[fake.addr, home.addr])), note));
    assert_eq!(resolved, Err(DnsError::NoSuchName(NAME.to_string())));
    assert!(system.calls().is_empty());
}

#[test]
fn no_records_of_one_type() {
    let turn = turn();
    let fake = home_server(&turn, vec![HOME], Vec::new());
    let system = FakeSystem::default();
    let r = resolver(&system, &loopback_ok);
    let (resolved, lines) = noted(|note| r.resolve(NAME, Some(&servers(&[fake.addr])), note));
    assert_eq!(
        resolved.unwrap().addrs,
        [found(IpAddr::V4(HOME), Source::Authoritative)]
    );
    assert!(system.calls().is_empty());
    assert!(has(&lines, "has no AAAA record"));

    let empty = home_server(&turn, Vec::new(), Vec::new());
    let (resolved, _) = noted(|note| r.resolve(NAME, Some(&servers(&[empty.addr])), note));
    assert_eq!(resolved, Err(DnsError::NoAddress(NAME.to_string())));
}

#[test]
fn cname_into_another_zone() {
    let turn = turn();
    let fake = Fake::start(&turn, |asked| {
        let reply = authoritative(vec![cname(&asked.name, NAME)]);
        vec![reply_to(asked, &reply)]
    });
    let system = FakeSystem::default().addrs("room.example.net", Kind::A, &["203.0.113.5"]);
    let r = resolver(&system, &loopback_ok);
    let (resolved, lines) =
        noted(|note| r.resolve("room.example.net", Some(&servers(&[fake.addr])), note));
    assert_eq!(
        resolved.unwrap().addrs,
        [found(IpAddr::V4(HOME), Source::System)]
    );
    assert_eq!(
        system.calls(),
        ["A room.example.net fresh", "AAAA room.example.net fresh"]
    );
    assert!(has(&lines, "points on to myroom.example.net"));
}

#[test]
fn referral_is_not_an_answer() {
    let turn = turn();
    let fake = Fake::start(&turn, |asked| {
        let reply = Reply {
            authority: vec![ns("example.net", "ns1.example.net")],
            ..Reply::default()
        };
        vec![reply_to(asked, &reply)]
    });
    let system = FakeSystem::default().addrs(NAME, Kind::A, &["203.0.113.5"]);
    let r = resolver(&system, &loopback_ok);
    let started = Instant::now();
    let (resolved, lines) = noted(|note| r.resolve(NAME, Some(&servers(&[fake.addr])), note));
    assert!(started.elapsed() < WAIT);
    assert_eq!(
        resolved.unwrap().addrs,
        [found(IpAddr::V4(HOME), Source::System)]
    );
    assert!(has(&lines, "it does not serve that name"));
}

#[test]
fn without_nameservers() {
    let system = FakeSystem::default()
        .addrs(NAME, Kind::A, &["203.0.113.5"])
        .addrs(NAME, Kind::Aaaa, &["2001:db8::5"]);
    let r = resolver(&system, &loopback_ok);
    for nameservers in [None, Some(servers(&[]))] {
        let (resolved, _) = noted(|note| r.resolve(NAME, nameservers.as_ref(), note));
        assert_eq!(
            resolved.unwrap().addrs,
            [
                found(IpAddr::V4(HOME), Source::System),
                found(IpAddr::V6(HOME_V6), Source::System),
            ]
        );
    }
    assert_eq!(
        system.calls(),
        [
            "A myroom.example.net fresh",
            "AAAA myroom.example.net fresh",
            "A myroom.example.net fresh",
            "AAAA myroom.example.net fresh",
        ]
    );
}

#[test]
fn refused_addresses_are_listed() {
    let turn = turn();
    let fake = home_server(
        &turn,
        vec![
            Ipv4Addr::LOCALHOST,
            HOME,
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::new(224, 0, 0, 1),
            HOME,
        ],
        vec![Ipv6Addr::LOCALHOST, HOME_V6],
    );
    let system = FakeSystem::default();
    // The invite's rules, except for the fake nameserver's own address.
    let check = |addr: SocketAddr| {
        if addr == fake.addr {
            Ok(())
        } else {
            like_invite(addr)
        }
    };
    let r = resolver(&system, &check);
    let (resolved, lines) = noted(|note| r.resolve(NAME, Some(&servers(&[fake.addr])), note));
    let resolved = resolved.unwrap();
    assert_eq!(
        resolved.addrs,
        [
            found(IpAddr::V4(HOME), Source::Authoritative),
            found(IpAddr::V6(HOME_V6), Source::Authoritative),
        ]
    );
    let refused = |text: &str, why: &'static str| Refused { ip: ip(text), why };
    assert_eq!(
        resolved.refused,
        [
            refused("127.0.0.1", "it is a loopback address"),
            refused("0.0.0.0", "it is the unspecified address"),
            refused("224.0.0.1", "it is a multicast address"),
            refused("::1", "it is a loopback address"),
        ]
    );
    assert!(has(&lines, "refused 127.0.0.1: it is a loopback address"));

    // The system resolver's addresses pass the same check, and a name that
    // only points at refused addresses still comes back with them listed.
    let system = FakeSystem::default().addrs(NAME, Kind::A, &["127.0.0.1"]);
    let r = resolver(&system, &like_invite);
    let (resolved, _) = noted(|note| r.resolve(NAME, None, note));
    let resolved = resolved.unwrap();
    assert!(resolved.addrs.is_empty());
    assert_eq!(
        resolved.refused,
        [refused("127.0.0.1", "it is a loopback address")]
    );
}

#[test]
fn nothing_answers() {
    let turn = turn();
    let fake = Fake::start(&turn, |_| Vec::new());
    let down = SystemError::Failed("the system resolver timed out".to_string());
    let system = FakeSystem::default()
        .addrs_fail(NAME, Kind::A, down.clone())
        .addrs_fail(NAME, Kind::Aaaa, down);
    let r = resolver(&system, &loopback_ok);
    let (resolved, lines) = noted(|note| r.resolve(NAME, Some(&servers(&[fake.addr])), note));
    assert_eq!(resolved, Err(DnsError::Unanswered(NAME.to_string())));
    assert!(has(&lines, "the system resolver timed out"));
}

#[test]
fn from_the_walk_to_the_answer() {
    let turn = turn();
    let fake = home_server(&turn, vec![HOME], Vec::new());
    let system = FakeSystem::default()
        .ns("example.net", &["ns1.example.net"])
        .addrs("ns1.example.net", Kind::A, &["127.0.0.1"]);
    let r = Resolver {
        port: fake.addr.port(),
        ..resolver(&system, &loopback_ok)
    };
    let (walked, _) = noted(|note| r.nameservers(NAME, note));
    let walked = walked.unwrap();
    assert_eq!(walked.addrs, [fake.addr]);
    let (resolved, lines) = noted(|note| r.resolve(NAME, Some(&walked), note));
    assert_eq!(
        resolved.unwrap().addrs,
        [found(IpAddr::V4(HOME), Source::Authoritative)]
    );
    assert!(has(
        &lines,
        &format!("asking {} for the A records of {NAME}", fake.addr)
    ));
}

#[test]
fn refused_nameservers_are_not_asked() {
    let turn = turn();
    let fake = home_server(&turn, vec![Ipv4Addr::new(198, 51, 100, 3)], Vec::new());
    let system = FakeSystem::default().addrs(NAME, Kind::A, &["203.0.113.5"]);
    let r = resolver(&system, &like_invite);
    let (resolved, lines) = noted(|note| r.resolve(NAME, Some(&servers(&[fake.addr])), note));
    assert_eq!(
        resolved.unwrap().addrs,
        [found(IpAddr::V4(HOME), Source::System)]
    );
    assert!(fake.seen().is_empty());
    assert!(has(
        &lines,
        &format!("refused nameserver {}: it is a loopback address", fake.addr)
    ));
}
