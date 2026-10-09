use std::fs;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV6};
use std::sync::atomic::{AtomicU32, Ordering};

use invite::CandidateKind;
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::sample::Index;

use super::format::{Malformed, VERSION, parse_devices, parse_hosts};
use super::*;

// A fresh folder under %TEMP% for one test, gone when it ends.
struct Folder(PathBuf);

impl Folder {
    fn new(test: &str) -> Folder {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "booth-known-{test}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("make a folder");
        Folder(path)
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.0)
            .expect("list the folder")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn candidate(kind: CandidateKind, addr: &str) -> Candidate {
    Candidate {
        kind,
        addr: addr.parse().unwrap(),
    }
}

fn device(n: u8) -> KnownDevice {
    KnownDevice {
        key: [n; 32],
        name: format!("Friend {n}"),
        first_seen: 1_790_000_000 + u64::from(n),
        last_seen: 1_790_100_000 + u64::from(n),
        secret: Zeroizing::new([n ^ 0x5a; 32]),
    }
}

// Every field in use: both families, a typed-in address, a name.
fn host(n: u8) -> KnownHost {
    KnownHost {
        host_key: [n; 32],
        room_name: format!("Room {n}"),
        host_name: String::from("Mara"),
        secret: Zeroizing::new([n ^ 0xa5; 32]),
        candidates: vec![
            candidate(CandidateKind::Lan, "192.168.1.20:41000"),
            candidate(CandidateKind::Vpn, "100.64.1.2:41000"),
            candidate(CandidateKind::Vpn, "[fd7a:115c::2]:41000"),
            candidate(CandidateKind::Ipv6, "[2001:db8::20]:41000"),
            candidate(CandidateKind::Public, "203.0.113.9:52000"),
        ],
        address_name: Some(String::from("myroom.example.net")),
        last_reached: Some("127.0.0.1:41007".parse().unwrap()),
        manual: Manual::parse("203.0.113.5:41000").unwrap(),
        last_seen: 1_790_200_000 + u64::from(n),
    }
}

fn full_devices() -> KnownDevices {
    KnownDevices {
        devices: vec![device(1), device(2)],
        blocked: vec![BlockedKey {
            key: [9; 32],
            since: 1_790_300_000,
        }],
    }
}

fn full_hosts() -> Vec<KnownHost> {
    let mut named = host(2);
    named.manual = Manual::parse("home.example.net").unwrap();
    named.address_name = None;
    named.last_reached = Some("[2001:db8::20]:41000".parse().unwrap());
    let mut bare = host(3);
    bare.candidates.clear();
    bare.manual = None;
    bare.last_reached = None;
    vec![host(1), named, bare]
}

fn same_device(a: &KnownDevice, b: &KnownDevice) {
    assert_eq!(a.key, b.key);
    assert_eq!(a.name, b.name);
    assert_eq!(a.first_seen, b.first_seen);
    assert_eq!(a.last_seen, b.last_seen);
    assert_eq!(*a.secret, *b.secret);
}

fn same_host(a: &KnownHost, b: &KnownHost) {
    assert_eq!(a.host_key, b.host_key);
    assert_eq!(a.room_name, b.room_name);
    assert_eq!(a.host_name, b.host_name);
    assert_eq!(*a.secret, *b.secret);
    assert_eq!(a.candidates, b.candidates);
    assert_eq!(a.address_name, b.address_name);
    assert_eq!(a.last_reached, b.last_reached);
    assert_eq!(a.manual, b.manual);
    assert_eq!(a.last_seen, b.last_seen);
}

#[test]
fn devices_round_trip_every_field() {
    let sent = full_devices();
    let back = parse_devices(&encode_devices(&sent)).expect("parses");
    assert_eq!(back.devices.len(), 2);
    for (a, b) in sent.devices.iter().zip(&back.devices) {
        same_device(a, b);
    }
    assert_eq!(back.blocked, sent.blocked);
    let empty = parse_devices(&encode_devices(&KnownDevices::default())).expect("parses");
    assert!(empty.devices.is_empty() && empty.blocked.is_empty());
}

#[test]
fn hosts_round_trip_every_field() {
    let sent = full_hosts();
    let back = parse_hosts(&encode_hosts(&sent)).expect("parses");
    assert_eq!(back.len(), sent.len());
    for (a, b) in sent.iter().zip(&back) {
        same_host(a, b);
    }
    assert!(parse_hosts(&encode_hosts(&[])).expect("parses").is_empty());
}

#[test]
fn every_cut_is_refused() {
    let devices = encode_devices(&full_devices());
    for len in 0..devices.len() {
        assert!(
            parse_devices(&devices[..len]).is_err(),
            "devices cut at {len}"
        );
    }
    let hosts = encode_hosts(&full_hosts());
    for len in 0..hosts.len() {
        assert!(parse_hosts(&hosts[..len]).is_err(), "hosts cut at {len}");
    }
}

#[test]
fn other_version_or_list_refused() {
    let mut devices = encode_devices(&full_devices()).to_vec();
    devices[4] = VERSION + 1;
    assert!(matches!(
        parse_devices(&devices),
        Err(Malformed::Version(v)) if v == VERSION + 1
    ));
    let mut hosts = encode_hosts(&full_hosts()).to_vec();
    hosts[4] = 0;
    assert!(matches!(parse_hosts(&hosts), Err(Malformed::Version(0))));

    let devices = encode_devices(&full_devices());
    let hosts = encode_hosts(&full_hosts());
    assert!(matches!(parse_hosts(&devices), Err(Malformed::Shape(_))));
    assert!(matches!(parse_devices(&hosts), Err(Malformed::Shape(_))));
    let mut trailing = hosts.to_vec();
    trailing.push(0);
    assert!(parse_hosts(&trailing).is_err());
}

#[test]
fn lists_past_caps() {
    // Distinct keys, one more than the cap.
    let mut distinct = KnownDevices::default();
    for n in 0..=MAX_DEVICES {
        let mut known = device(1);
        known.key[..2].copy_from_slice(&(n as u16).to_le_bytes());
        distinct.devices.push(known);
    }
    let written = parse_devices(&encode_devices(&distinct)).expect("the writer keeps to the cap");
    assert_eq!(written.devices.len(), MAX_DEVICES);

    // The count itself, one over.
    let mut over = encode_devices(&KnownDevices::default()).to_vec();
    over[5..7].copy_from_slice(&(MAX_DEVICES as u16 + 1).to_le_bytes());
    assert!(parse_devices(&over).is_err());
    let mut over = encode_devices(&KnownDevices::default()).to_vec();
    over[7..9].copy_from_slice(&(MAX_BLOCKED as u16 + 1).to_le_bytes());
    assert!(parse_devices(&over).is_err());

    let hosts: Vec<KnownHost> = (0..=MAX_HOSTS as u8).map(host).collect();
    let written = parse_hosts(&encode_hosts(&hosts)).expect("the writer keeps to the cap");
    assert_eq!(written.len(), MAX_HOSTS);
    let mut over = encode_hosts(&[]).to_vec();
    over[5] = MAX_HOSTS as u8 + 1;
    assert!(parse_hosts(&over).is_err());
}

#[test]
fn a_key_twice_is_refused_and_never_written() {
    let twice = KnownDevices {
        devices: vec![device(1), device(1)],
        blocked: vec![BlockedKey {
            key: [1; 32],
            since: 5,
        }],
    };
    let written = parse_devices(&encode_devices(&twice)).expect("parses");
    assert!(written.devices.is_empty(), "a blocked key is not a device");
    assert_eq!(written.blocked.len(), 1);

    let mut bytes = encode_devices(&KnownDevices {
        devices: vec![device(1), device(2)],
        blocked: Vec::new(),
    })
    .to_vec();
    // The second device's key, made the first's.
    let second = 5 + 2 + 32 + 32 + 8 + 8 + 1 + device(1).name.len();
    bytes[second..second + 32].copy_from_slice(&[1; 32]);
    assert!(parse_devices(&bytes).is_err());

    let hosts = vec![host(1), host(1)];
    assert_eq!(parse_hosts(&encode_hosts(&hosts)).expect("parses").len(), 1);
}

#[test]
fn only_what_the_rules_allow_is_written() {
    let mut odd = host(4);
    odd.room_name = String::from("  \u{202E}Tuesday\u{200B} night ");
    odd.host_name = String::new();
    odd.candidates = vec![
        candidate(CandidateKind::Public, "192.168.1.20:41000"),
        candidate(CandidateKind::Lan, "127.0.0.1:41000"),
        candidate(CandidateKind::Lan, "192.168.1.20:41000"),
        candidate(CandidateKind::Lan, "192.168.1.20:41000"),
        Candidate {
            kind: CandidateKind::Vpn,
            addr: SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::new(0xfd7a, 0x115c, 0, 0, 0, 0, 0, 2),
                41000,
                7,
                3,
            )),
        },
    ];
    odd.address_name = Some(String::from("localhost"));
    odd.last_reached = Some("0.0.0.0:41000".parse().unwrap());
    let back = parse_hosts(&encode_hosts(&[odd])).expect("parses");
    assert_eq!(back[0].room_name, "Tuesday night");
    assert_eq!(back[0].host_name, crate::control::PERSON_FALLBACK);
    assert_eq!(
        back[0].candidates,
        [
            candidate(CandidateKind::Lan, "192.168.1.20:41000"),
            candidate(CandidateKind::Vpn, "[fd7a:115c::2]:41000"),
        ]
    );
    assert_eq!(back[0].address_name, None);
    assert_eq!(back[0].last_reached, None);
}

#[test]
fn unclean_name_refused() {
    let mut bytes = encode_devices(&KnownDevices {
        devices: vec![device(1)],
        blocked: Vec::new(),
    })
    .to_vec();
    let name_at = 5 + 2 + 32 + 32 + 8 + 8 + 1;
    bytes[name_at] = b' ';
    assert!(parse_devices(&bytes).is_err());
}

#[test]
fn what_can_be_typed_for_a_host() {
    let addr = |text: &str| Some(Manual(Entry::Addr(text.parse().unwrap())));
    assert_eq!(
        Manual::parse(" 203.0.113.5:41000 "),
        Ok(addr("203.0.113.5:41000"))
    );
    assert_eq!(
        Manual::parse("[2001:db8::5]:41000"),
        Ok(addr("[2001:db8::5]:41000"))
    );
    assert_eq!(
        Manual::parse("myroom.example.net"),
        Ok(Some(Manual(Entry::Name(String::from(
            "myroom.example.net"
        )))))
    );
    assert_eq!(Manual::parse("   "), Ok(None));
    for bad in [
        "203.0.113.5",
        "203.0.113.5:0",
        "127.0.0.1:41000",
        "[::1]:41000",
        "localhost",
        "my room.example.net",
        "myroom.example.net:41000",
        "2001:db8::5",
        "https://myroom.example.net",
    ] {
        assert!(Manual::parse(bad).is_err(), "{bad}");
    }
    let typed = Manual::parse("myroom.example.net").unwrap().unwrap();
    assert_eq!(typed.to_string(), "myroom.example.net");
}

// Each edit replaces, inserts or removes one byte.
fn edited(mut bytes: Vec<u8>, edits: Vec<(Index, u8, u8)>) -> Vec<u8> {
    for (at, byte, op) in edits {
        match op {
            0 if !bytes.is_empty() => {
                let i = at.index(bytes.len());
                bytes[i] = byte;
            }
            1 => bytes.insert(at.index(bytes.len() + 1), byte),
            _ if !bytes.is_empty() => {
                bytes.remove(at.index(bytes.len()));
            }
            _ => {}
        }
    }
    bytes
}

fn any_addr() -> impl Strategy<Value = SocketAddr> {
    prop_oneof![
        (any::<[u8; 4]>(), any::<u16>()).prop_map(|(ip, port)| SocketAddr::from((ip, port))),
        (any::<[u16; 8]>(), any::<u16>()).prop_map(|(ip, port)| SocketAddr::from((ip, port))),
        (0..3u8, any::<u16>()).prop_map(|(n, port)| SocketAddr::from(([203, 0, 113, n], port))),
    ]
}

fn any_kind() -> impl Strategy<Value = CandidateKind> {
    prop_oneof![
        Just(CandidateKind::Lan),
        Just(CandidateKind::Vpn),
        Just(CandidateKind::Ipv6),
        Just(CandidateKind::Public),
    ]
}

fn any_name() -> impl Strategy<Value = Option<String>> {
    prop_oneof![
        Just(None),
        Just(Some(String::from("myroom.example.net"))),
        "[a-z0-9.:-]{0,20}".prop_map(Some),
    ]
}

prop_compose! {
    fn any_host()(
        key in any::<[u8; 32]>(),
        room in ".{0,80}",
        name in ".{0,80}",
        candidates in vec((any_kind(), any_addr()), 0..20),
        address_name in any_name(),
        last_reached in proptest::option::of(any_addr()),
        typed in any_name(),
        last_seen in any::<u64>(),
    ) -> KnownHost {
        KnownHost {
            host_key: key,
            room_name: room,
            host_name: name,
            secret: Zeroizing::new([7; 32]),
            candidates: candidates
                .into_iter()
                .map(|(kind, addr)| Candidate { kind, addr })
                .collect(),
            address_name,
            last_reached,
            manual: typed.and_then(|text| Manual::parse(&text).ok().flatten()),
            last_seen,
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    #[test]
    fn random_bytes_parse_back_exactly(
        body in vec(any::<u8>(), 0..400),
        tag in 0..3u8,
    ) {
        let mut bytes = match tag {
            0 => b"BDEV\x01".to_vec(),
            1 => b"BHST\x01".to_vec(),
            _ => Vec::new(),
        };
        bytes.extend(body);
        if let Ok(list) = parse_devices(&bytes) {
            prop_assert_eq!(encode_devices(&list).to_vec(), bytes.to_vec());
        }
        if let Ok(hosts) = parse_hosts(&bytes) {
            prop_assert_eq!(encode_hosts(&hosts).to_vec(), bytes.to_vec());
        }
    }

    #[test]
    fn edited_lists_parse_back_exactly(
        edits in vec((any::<Index>(), any::<u8>(), 0..3u8), 1..8),
    ) {
        let bytes = edited(encode_hosts(&full_hosts()).to_vec(), edits.clone());
        if let Ok(hosts) = parse_hosts(&bytes) {
            prop_assert_eq!(encode_hosts(&hosts).to_vec(), bytes.to_vec());
        }
        let bytes = edited(encode_devices(&full_devices()).to_vec(), edits);
        if let Ok(list) = parse_devices(&bytes) {
            prop_assert_eq!(encode_devices(&list).to_vec(), bytes.to_vec());
        }
    }

    #[test]
    fn whatever_is_written_reads_back(hosts in vec(any_host(), 0..4)) {
        let bytes = encode_hosts(&hosts);
        let back = parse_hosts(&bytes);
        prop_assert!(back.is_ok(), "{:?}", back.err());
        prop_assert_eq!(encode_hosts(&back.unwrap()).to_vec(), bytes.to_vec());
    }
}

#[test]
fn a_missing_file_is_an_empty_list() {
    let folder = Folder::new("missing");
    let opened = open_hosts(&folder.0);
    assert!(opened.list.is_empty() && opened.problem.is_none() && opened.writable);
    assert!(hosts(&folder.0).unwrap().is_empty());
    assert!(devices(&folder.0).unwrap().devices.is_empty());
    assert!(folder.names().is_empty(), "reading made a file");
}

#[test]
fn unreadable_list_put_aside() {
    let folder = Folder::new("damaged");
    let path = folder.0.join(List::Hosts.file_name());
    keys::write_protected(&path, b"not a list", "Booth test").unwrap();
    let err = hosts(&folder.0).expect_err("damaged");
    assert_eq!(
        err.damaged(),
        Some(DamagedList {
            list: List::Hosts,
            kept_as: String::from("hosts.bin.bad"),
        })
    );
    assert!(
        err.to_string().contains("it is not a list of known hosts"),
        "{err}"
    );
    assert!(hosts(&folder.0).unwrap().is_empty());
    assert_eq!(folder.names(), ["hosts.bin.bad"]);

    // Not a DPAPI file at all, and a second one: the first copy stays.
    fs::write(folder.0.join("devices.bin"), b"junk").unwrap();
    fs::write(&path, b"junk").unwrap();
    let err = devices(&folder.0).expect_err("damaged");
    assert_eq!(err.damaged().unwrap().kept_as, "devices.bin.bad");
    let err = hosts(&folder.0).expect_err("damaged");
    assert_eq!(err.damaged().unwrap().kept_as, "hosts.bin.bad2");
    assert_eq!(
        folder.names(),
        ["devices.bin.bad", "hosts.bin.bad", "hosts.bin.bad2"]
    );
}

#[test]
fn stuck_list_never_written_over() {
    let folder = Folder::new("stuck");
    for n in ["", "2", "3", "4", "5", "6", "7", "8", "9"] {
        fs::write(folder.0.join(format!("hosts.bin.bad{n}")), b"older").unwrap();
    }
    fs::write(folder.0.join("hosts.bin"), b"junk").unwrap();
    let opened = open_hosts(&folder.0);
    assert!(!opened.writable);
    let err = opened.problem.expect("stuck");
    assert!(matches!(err, KnownError::Stuck { .. }));
    assert!(matches!(
        err.problem(),
        Some(ListProblem::Unusable {
            list: List::Hosts,
            ..
        })
    ));
    // The file is named once, and the line ends with what to do.
    let text = err.to_string();
    assert_eq!(text.matches("hosts.bin").count(), 1, "{text}");
    assert!(
        text.contains("could not be put aside: .bad to .bad9 are all taken;"),
        "{text}"
    );
    assert!(text.ends_with("then start Booth again"), "{text}");
    assert!(forget_host(&folder.0, &[1; 32]).is_err());
    assert_eq!(fs::read(folder.0.join("hosts.bin")).unwrap(), b"junk");
}

// Going back to an older Booth for a night must not cost the newer one its
// list: the file stays where it is, and nothing is written over it.
#[test]
fn a_list_from_a_newer_booth_is_left_alone() {
    let folder = Folder::new("newer");
    let mut newer = encode_hosts(&full_hosts()).to_vec();
    newer[4] = VERSION + 1;
    save(&folder.0, List::Hosts, &newer).unwrap();
    let err = forget_host(&folder.0, &[1; 32]).expect_err("a newer list");
    assert!(
        matches!(
            err,
            KnownError::Newer {
                list: List::Hosts,
                ..
            }
        ),
        "{err}"
    );
    assert!(
        err.to_string().ends_with("run the newest version of Booth"),
        "{err}"
    );
    assert!(matches!(
        err.problem(),
        Some(ListProblem::Unusable {
            list: List::Hosts,
            ..
        })
    ));
    let opened = open_hosts(&folder.0);
    assert!(!opened.writable && opened.list.is_empty());
    let kept = keys::read_protected(&folder.0.join("hosts.bin")).unwrap();
    assert_eq!(kept.as_slice(), newer.as_slice(), "untouched");

    // The file around the list in a format this Booth does not know.
    let mut outer = b"BOOTHKEY".to_vec();
    outer.push(2);
    outer.extend_from_slice(b"from later");
    fs::write(folder.0.join("devices.bin"), &outer).unwrap();
    assert!(matches!(
        devices(&folder.0),
        Err(KnownError::Newer {
            list: List::Devices,
            ..
        })
    ));
    assert_eq!(fs::read(folder.0.join("devices.bin")).unwrap(), outer);
    assert_eq!(folder.names(), ["devices.bin", "hosts.bin"]);

    // No Booth ever wrote format 0, so that one is damaged.
    let mut zero = encode_devices(&full_devices()).to_vec();
    zero[4] = 0;
    save(&folder.0, List::Devices, &zero).unwrap();
    let err = devices(&folder.0).expect_err("damaged");
    assert_eq!(err.damaged().unwrap().kept_as, "devices.bin.bad");
    assert!(matches!(err.problem(), Some(ListProblem::Damaged(_))));
}

#[test]
fn a_failed_write_is_no_start_screen_line() {
    let err = KnownError::Write(keys::KeyError::NoLocalAppData);
    assert_eq!(err.problem(), None);
}

#[test]
fn unopenable_list_left_alone() {
    let folder = Folder::new("unreadable");
    // Windows will not open a folder as a file.
    fs::create_dir(folder.0.join("devices.bin")).unwrap();
    let opened = open_devices(&folder.0);
    assert!(!opened.writable);
    let err = opened.problem.expect("unreadable");
    assert!(matches!(
        err,
        KnownError::Read {
            list: List::Devices,
            ..
        }
    ));
    assert!(matches!(
        err.problem(),
        Some(ListProblem::Unusable {
            list: List::Devices,
            ..
        })
    ));
    assert!(remove_device(&folder.0, &[1; 32]).is_err());
    assert_eq!(folder.names(), ["devices.bin"]);
}

#[test]
fn panel_forgets_and_sets_manual() {
    let folder = Folder::new("panel-hosts");
    save(&folder.0, List::Hosts, &encode_hosts(&full_hosts())).unwrap();
    let listed = hosts(&folder.0).unwrap();
    let seen: Vec<u64> = listed.iter().map(KnownHost::last_seen).collect();
    assert_eq!(
        seen,
        [1_790_200_003, 1_790_200_002, 1_790_200_001],
        "newest first"
    );

    let typed = Manual::parse("myroom.example.net").unwrap();
    set_manual(&folder.0, &[1; 32], typed.clone()).unwrap();
    forget_host(&folder.0, &[2; 32]).unwrap();
    // Forgetting a host that is not there changes nothing.
    forget_host(&folder.0, &[8; 32]).unwrap();
    let listed = hosts(&folder.0).unwrap();
    assert_eq!(listed.len(), 2);
    let first = listed.iter().find(|h| h.host_key == [1; 32]).unwrap();
    assert_eq!(first.manual, typed);
    set_manual(&folder.0, &[1; 32], None).unwrap();
    assert_eq!(hosts(&folder.0).unwrap()[1].manual, None);
    assert_eq!(folder.names(), ["hosts.bin"]);
}

#[test]
fn panel_removes_and_unblocks() {
    let folder = Folder::new("panel-devices");
    save(&folder.0, List::Devices, &encode_devices(&full_devices())).unwrap();
    let listed = devices(&folder.0).unwrap();
    assert_eq!(listed.devices[0].key, [2; 32], "newest first");
    remove_device(&folder.0, &[1; 32]).unwrap();
    unblock(&folder.0, &[9; 32]).unwrap();
    let listed = devices(&folder.0).unwrap();
    assert_eq!(listed.devices.len(), 1);
    assert_eq!(listed.devices[0].key, [2; 32]);
    assert!(listed.blocked.is_empty());
}

#[test]
fn full_device_list_drops_oldest() {
    let mut list = KnownDevices::default();
    for n in 0..MAX_DEVICES {
        let mut known = device(1);
        known.key[..2].copy_from_slice(&(n as u16).to_le_bytes());
        known.last_seen = 1000 + n as u64;
        list.devices.push(known);
    }
    let mut book = DeviceBook::new(list, true);
    let oldest = book.devices()[0].key;
    let second = book.devices()[1].key;
    let secret = Zeroizing::new([3; 32]);
    let in_room = |key: &[u8; 32]| *key == oldest;
    assert_eq!(
        book.joined([0xee; 32], &secret, 5000, in_room),
        Joined::AddedInPlaceOf(second),
        "the oldest is in the room, so the next one goes"
    );
    assert!(book.is_known(&[0xee; 32]));
    assert_eq!(
        book.joined([0xee; 32], &secret, 6000, in_room),
        Joined::Again
    );
    let save = book.take_save(Instant::now()).expect("changed");
    assert_eq!(
        parse_devices(&save.bytes).unwrap().devices.len(),
        MAX_DEVICES
    );
    assert!(
        book.take_save(Instant::now()).is_none(),
        "nothing changed since"
    );

    let mut unreadable = DeviceBook::new(KnownDevices::default(), false);
    assert_eq!(
        unreadable.joined([1; 32], &secret, 5000, |_| false),
        Joined::NotKept
    );
    assert!(unreadable.take_last_save().is_none());
}

// A friend's program can send a new name in every packet. The room encodes
// the list for the saver once a second at most, and the timer thread is
// told when the change held back is due.
#[test]
fn saves_at_most_once_a_second() {
    let start = Instant::now();
    let mut book = DeviceBook::new(full_devices(), true);
    assert!(book.take_save(start).is_none(), "nothing changed yet");
    assert_eq!(book.save_due(), None);
    book.named(&[1; 32], "Ana");
    assert!(book.take_save(start).is_some());
    for n in 0..50u64 {
        book.named(&[1; 32], &format!("Ana {n}"));
        let at = start + Duration::from_millis(n * 10);
        assert!(book.take_save(at).is_none(), "handed over again at {n}");
    }
    assert_eq!(book.save_due(), Some(start + SAVE_GAP));
    let save = book
        .take_save(start + SAVE_GAP)
        .expect("its second has come");
    let saved = parse_devices(&save.bytes).unwrap();
    let ana = saved.devices.iter().find(|d| d.key == [1; 32]).unwrap();
    assert_eq!(ana.name, "Ana 49", "the newest name");
    assert_eq!(book.save_due(), None, "nothing waits");

    // Closing takes the last change at once.
    book.named(&[1; 32], "Ana");
    assert!(book.take_save(start + SAVE_GAP).is_none());
    assert!(book.take_last_save().is_some());
    assert!(book.take_last_save().is_none());
    assert_eq!(book.save_due(), None);

    // A list that cannot be written is never due, or the timer thread
    // would wake for it forever.
    let mut unwritable = DeviceBook::new(full_devices(), false);
    unwritable.named(&[1; 32], "Ana");
    assert!(unwritable.take_save(start).is_none());
    unwritable.named(&[1; 32], "Ana 2");
    assert_eq!(unwritable.save_due(), None);
}

// The whole list has room from the start, so a join never moves the
// secrets to a new buffer and leaves them in the old one.
#[test]
fn room_list_never_grows() {
    let book = DeviceBook::new(full_devices(), true);
    assert!(book.list.devices.capacity() >= MAX_DEVICES);
    let book = HostBook::new(full_hosts(), true);
    assert!(book.hosts.capacity() >= MAX_HOSTS);
}

// A room's last save can land after leave returned. A device removed in
// settings in between stays removed.
#[test]
fn late_room_save_left_out() {
    let folder = Folder::new("turns");
    save(&folder.0, List::Devices, &encode_devices(&full_devices())).unwrap();
    let (opened, turn) = room_devices(&folder.0);
    assert_eq!(opened.list.devices.len(), 2);
    let room_list = encode_devices(&opened.list);
    remove_device(&folder.0, &[1; 32]).unwrap();
    assert!(matches!(turn.save(&room_list), Ok(Saved::Passed)));
    assert_eq!(devices(&folder.0).unwrap().devices.len(), 1);

    // Nothing changed in settings: the room's save goes in.
    let (opened, turn) = room_devices(&folder.0);
    unblock(&folder.0, &[8; 32]).unwrap();
    assert!(matches!(
        turn.save(&encode_devices(&opened.list)),
        Ok(Saved::Written)
    ));

    // A room opened after another takes the list from it.
    let (_, first) = room_devices(&folder.0);
    let (_, second) = room_devices(&folder.0);
    assert!(matches!(first.save(&room_list), Ok(Saved::Passed)));
    assert!(matches!(second.save(&room_list), Ok(Saved::Written)));
    assert_eq!(devices(&folder.0).unwrap().devices.len(), 2);

    // Each file has its own turn.
    let (_, hosts_turn) = room_hosts(&folder.0);
    remove_device(&folder.0, &[2; 32]).unwrap();
    assert!(matches!(
        hosts_turn.save(&encode_hosts(&full_hosts())),
        Ok(Saved::Written)
    ));
}

#[test]
fn full_host_list_drops_oldest() {
    let hosts: Vec<KnownHost> = (0..MAX_HOSTS as u8).map(host).collect();
    let mut book = HostBook::new(hosts, true);
    let gone = book.add(host(200)).expect("one made way");
    assert_eq!(gone.host_key, [0; 32]);
    assert!(book.get(&[200; 32]).is_some());
    let save = book.take_last_save().expect("changed");
    assert_eq!(parse_hosts(&save.bytes).unwrap().len(), MAX_HOSTS);
}

#[test]
fn the_rule_for_candidates_is_the_invites() {
    assert!(usable(&candidate(CandidateKind::Lan, "192.168.1.20:41000")));
    assert!(usable(&candidate(
        CandidateKind::Public,
        "203.0.113.9:41000"
    )));
    assert!(!usable(&candidate(
        CandidateKind::Public,
        "192.168.1.20:41000"
    )));
    assert!(!usable(&candidate(CandidateKind::Lan, "127.0.0.1:41000")));
    assert!(!usable(&candidate(
        CandidateKind::Ipv6,
        "203.0.113.9:41000"
    )));
    assert!(reachable("127.0.0.1:41000".parse().unwrap()));
    assert!(!reachable("0.0.0.0:41000".parse().unwrap()));
    assert!(!reachable(SocketAddr::from((Ipv4Addr::BROADCAST, 41000))));
    assert!(!reachable("192.168.1.20:0".parse().unwrap()));
}
