use super::*;
use crate::known::KnownDevices;
use crate::mapper::Protocol;
use crate::testing::{Wire, ping, seal};
use crate::view::InviteView;
use channels::{ClockSample, Reliable};
use invite::{CandidateKind, Invite};
use session::{COOKIE_REPLY_LEN, Initiation, TimestampSource};
use std::net::{Ipv4Addr, SocketAddrV4, SocketAddrV6};

struct Rig {
    host: Host,
    socket: Socket,
    invite: Invite,
}

impl Rig {
    // A host named Mara with no room name, holding a multi-use invite.
    fn new(now: Instant) -> Rig {
        Rig::with_log(now, Log::off())
    }

    fn with_log(now: Instant, log: Log) -> Rig {
        Rig::with_timers(now, log, Timers::default())
    }

    fn with_timers(now: Instant, log: Log, timers: Timers) -> Rig {
        let identity = Arc::new(Identity::generate());
        let devices = DeviceBook::new(KnownDevices::default(), true);
        let (mut host, socket) = Rig::build(now, log, timers, false, identity, devices, Vec::new());
        host.stun_resolved(now);
        host.new_invite(true, now);
        Rig::holding(host, socket, now)
    }

    // No STUN servers, so STUN settles as soon as it is told so.
    fn host(now: Instant, log: Log, asking_router: bool) -> (Host, Socket) {
        let identity = Arc::new(Identity::generate());
        let devices = DeviceBook::new(KnownDevices::default(), true);
        Rig::build(
            now,
            log,
            Timers::default(),
            asking_router,
            identity,
            devices,
            Vec::new(),
        )
    }

    // A host opened again, with the list an earlier one saved.
    fn reopened(now: Instant, identity: Arc<Identity>, devices: KnownDevices, log: Log) -> Rig {
        let devices = DeviceBook::new(devices, true);
        let timers = Timers::default();
        let (mut host, socket) = Rig::build(now, log, timers, false, identity, devices, Vec::new());
        host.stun_resolved(now);
        host.new_invite(true, now);
        Rig::holding(host, socket, now)
    }

    fn build(
        now: Instant,
        log: Log,
        timers: Timers,
        asking_router: bool,
        identity: Arc<Identity>,
        devices: DeviceBook,
        local: Vec<Candidate>,
    ) -> (Host, Socket) {
        let socket = Socket::bind(0, Log::off()).expect("bind the host socket");
        let host = Host::new(HostSetup {
            identity,
            name: "Mara".to_owned(),
            room_name: String::new(),
            timers,
            local: Local::Fixed(local),
            port: socket.local_port(),
            has_ipv6: socket.has_ipv6(),
            router: asking_router.then_some(Gateway {
                ip: Ipv4Addr::LOCALHOST,
                local: Ipv4Addr::LOCALHOST,
            }),
            punch_loopback: true,
            version: invite::VERSION,
            address_name: None,
            lookup: Lookup::default(),
            devices,
            list_problem: None,
            initiations_read: Arc::default(),
            voice: crate::testing::quiet_voice(true),
            speaker: crate::testing::no_speaker(),
            screen: crate::testing::quiet_screen(),
            now,
            log,
        });
        (host, socket)
    }

    // The rig around a host that already shows an invite.
    fn holding(host: Host, socket: Socket, now: Instant) -> Rig {
        let code = host.view(now, 0).invite.expect("an invite").code;
        let invite = Invite::decode(&code).expect("the code decodes");
        Rig {
            host,
            socket,
            invite,
        }
    }

    fn deliver(&mut self, packet: &[u8], from: SocketAddr, now: Instant) {
        self.host.on_packet(packet, from, now, &self.socket);
    }

    fn tick(&mut self, now: Instant) {
        self.host.on_timer(now, &self.socket);
    }
}

#[derive(Debug)]
enum Took {
    Control(Message),
    Chat(ChatMessage),
}

// A client played by hand.
struct Guest {
    identity: Identity,
    wire: Wire,
    stamps: TimestampSource,
    session: Option<Session>,
    reliable: Reliable,
    chat: Reliable,
    feedback: Reliable,
}

impl Guest {
    fn new() -> Guest {
        Guest {
            identity: Identity::generate(),
            wire: Wire::new(),
            stamps: TimestampSource::new(),
            session: None,
            reliable: Reliable::new(),
            chat: Reliable::new(),
            feedback: Reliable::new(),
        }
    }

    fn invite_psk(&self, invite: &Invite) -> Zeroizing<[u8; 32]> {
        session::invite_psk(&invite.secret, &invite.host_key, self.identity.public())
    }

    fn initiate(
        &mut self,
        rig: &mut Rig,
        kind: InitKind,
        psk: &[u8; 32],
        from: SocketAddr,
        now: Instant,
    ) -> Initiation {
        self.initiate_with(rig, kind, psk, from, None, now)
    }

    fn initiate_with(
        &mut self,
        rig: &mut Rig,
        kind: InitKind,
        psk: &[u8; 32],
        from: SocketAddr,
        cookie: Option<&[u8; 16]>,
        now: Instant,
    ) -> Initiation {
        let (initiation, packet) = Initiation::start_with_cookie(
            &self.identity.private_bytes(),
            self.identity.public(),
            &rig.invite.host_key,
            psk,
            kind,
            self.stamps.next_stamp(),
            peer::random_index(|_| false),
            cookie,
        )
        .expect("start an initiation");
        rig.deliver(&packet, from, now);
        initiation
    }

    fn knock(&mut self, rig: &mut Rig, now: Instant) -> Initiation {
        self.knock_with(rig, None, now)
    }

    fn knock_with(&mut self, rig: &mut Rig, cookie: Option<&[u8; 16]>, now: Instant) -> Initiation {
        let psk = self.invite_psk(&rig.invite);
        let kind = InitKind::Invite(rig.invite.invite_id);
        self.initiate_with(rig, kind, &psk, self.wire.addr(), cookie, now)
    }

    // The session from the host's answer, if it answered.
    fn answer(&mut self, mut initiation: Initiation, now: Instant) -> Option<Session> {
        self.wire
            .packets()
            .iter()
            .find_map(|packet| initiation.finish(packet, now).ok())
    }

    fn join(&mut self, rig: &mut Rig, name: &str, now: Instant) {
        self.join_saying(rig, name, None, now);
    }

    fn join_saying(
        &mut self,
        rig: &mut Rig,
        name: &str,
        reached: Option<SocketAddr>,
        now: Instant,
    ) {
        self.join_as(rig, name, invite::VERSION, reached, now);
    }

    fn join_as(
        &mut self,
        rig: &mut Rig,
        name: &str,
        version: Version,
        reached: Option<SocketAddr>,
        now: Instant,
    ) {
        let initiation = self.knock(rig, now);
        self.session = Some(self.answer(initiation, now).expect("the host answers"));
        let hello = Message::Hello {
            version,
            name: name.to_owned(),
            reached,
        };
        self.say(rig, &hello, now);
    }

    fn say(&mut self, rig: &mut Rig, message: &Message, now: Instant) {
        self.reliable.send(&message.encode()).expect("queued");
        while let Some(frame) = self.reliable.poll_transmit(now, None) {
            let session = self.session.as_mut().expect("joined");
            let packet = seal(session, Channel::Control, &frame);
            rig.deliver(&packet, self.wire.addr(), now);
        }
    }

    fn say_feedback(&mut self, rig: &mut Rig, message: &Message, now: Instant) {
        self.feedback.send(&message.encode()).expect("queued");
        while let Some(frame) = self.feedback.poll_transmit(now, None) {
            let session = self.session.as_mut().expect("joined");
            let packet = seal(session, Channel::Feedback, &frame);
            rig.deliver(&packet, self.wire.addr(), now);
        }
    }

    // Any bytes on the chat stream, as a friend's program could send.
    fn chat(&mut self, rig: &mut Rig, bytes: &[u8], now: Instant) {
        self.chat.send(bytes).expect("queued");
        self.send_chat(rig, now);
    }

    // What the chat stream has for the host: lines the window lets out,
    // and acks for what was read.
    fn send_chat(&mut self, rig: &mut Rig, now: Instant) {
        while let Some(frame) = self.chat.poll_transmit(now, None) {
            let session = self.session.as_mut().expect("joined");
            let packet = seal(session, Channel::Chat, &frame);
            rig.deliver(&packet, self.wire.addr(), now);
        }
    }

    // Everything the host hands on, however much: what it holds back
    // for want of window follows each ack.
    fn said_all(&mut self, rig: &mut Rig, now: Instant) -> Vec<ChatMessage> {
        let mut out = Vec::new();
        loop {
            let got = self.said(now);
            if got.is_empty() {
                return out;
            }
            out.extend(got);
            self.send_chat(rig, now);
        }
    }

    // What arrived since the last look, into the two streams.
    fn read(&mut self, now: Instant) {
        for packet in self.wire.packets() {
            self.take(&packet, now);
        }
    }

    fn take(&mut self, packet: &[u8], now: Instant) {
        let mut plain = Vec::new();
        let Some(session) = self.session.as_mut() else {
            return;
        };
        if session.decrypt(packet, &mut plain).is_err() {
            return;
        }
        match peer::read_plain(&plain) {
            Some(Plain::Control(frame)) => {
                let _ = self.reliable.receive(frame, now);
            }
            Some(Plain::Chat(frame)) => {
                let _ = self.chat.receive(frame, now);
            }
            Some(Plain::Feedback(frame)) => {
                let _ = self.feedback.receive(frame, now);
            }
            _ => {}
        }
    }

    // The control messages the host sent since the last look, then the
    // video feedback.
    fn heard(&mut self, now: Instant) -> Vec<Message> {
        self.read(now);
        let mut out = Vec::new();
        while let Some(bytes) = self.reliable.next_delivered() {
            out.extend(Message::decode(&bytes));
        }
        while let Some(bytes) = self.feedback.next_delivered() {
            out.extend(Message::decode(&bytes));
        }
        out
    }

    // Both streams since the last look, in the order a build takes them.
    fn took(&mut self, now: Instant) -> Vec<Took> {
        let mut out = Vec::new();
        for packet in self.wire.packets() {
            self.take(&packet, now);
            while let Some(bytes) = self.reliable.next_delivered() {
                out.extend(Message::decode(&bytes).map(Took::Control));
            }
            while let Some(bytes) = self.chat.next_delivered() {
                out.extend(ChatMessage::decode(&bytes).map(Took::Chat));
            }
        }
        out
    }

    // The chat the host handed on since the last look.
    fn said(&mut self, now: Instant) -> Vec<ChatMessage> {
        self.read(now);
        let mut out = Vec::new();
        while let Some(bytes) = self.chat.next_delivered() {
            out.extend(ChatMessage::decode(&bytes));
        }
        out
    }

    // Back from a closed room: no session, and reliable streams that
    // start over with the next one.
    fn left(&mut self) {
        self.session = None;
        self.reliable = Reliable::new();
        self.chat = Reliable::new();
        self.feedback = Reliable::new();
        self.wire.packets();
    }
}

fn addresses_in(messages: &[Message]) -> Vec<(Vec<Candidate>, Option<String>)> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::HostAddresses {
                candidates,
                address_name,
            } => Some((candidates.clone(), address_name.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn device_rejoins_after_restart() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let save = rig.host.take_last_save().expect("the join is saved");
    let saved = known::parse_devices(&save.bytes).expect("what was saved reads back");
    assert_eq!(saved.devices.len(), 1);
    let kept = saved.devices[0].clone();
    assert_eq!(kept.key, *ana.identity.public());
    assert_eq!(kept.name, "Ana");
    let secret = rig.host.keys[ana.identity.public()]
        .secret
        .clone()
        .expect("a secret");
    assert_eq!(*kept.secret, *secret);
    assert!(rig.host.take_last_save().is_none(), "nothing changed since");

    // The host closes, and a new room opens from what it saved. Ana
    // comes back with the secret and no invite.
    let identity = Arc::clone(&rig.host.identity);
    let later = start + Duration::from_secs(60);
    rig.host.leave(later, &rig.socket);
    let mut again = Rig::reopened(later, identity, saved, Log::off());
    ana.left();
    let from = ana.wire.addr();
    let rejoin = ana.initiate(&mut again, InitKind::Known, &secret, from, later);
    let session = ana.answer(rejoin, later);
    ana.session = Some(session.expect("a known device is answered after a restart"));
    let hello = Message::Hello {
        version: invite::VERSION,
        name: "Ana".to_owned(),
        reached: None,
    };
    ana.say(&mut again, &hello, later);
    assert_eq!(again.host.peers.len(), 1);
    let view = again.host.view(later, 0);
    assert!(!view.people[1].joined_by_invite, "no fingerprint this time");
    let heard = ana.heard(later);
    assert!(
        heard
            .iter()
            .any(|m| matches!(m, Message::PeerSecret { secret: s } if **s == *secret)),
        "{heard:?}"
    );
    let seen = known::parse_devices(&again.host.take_last_save().expect("seen again").bytes)
        .expect("reads back");
    assert!(seen.devices[0].last_seen >= kept.last_seen);
}

#[test]
fn a_blocked_key_gets_no_answer() {
    let start = Instant::now();
    let mut ana = Guest::new();
    let devices = KnownDevices {
        devices: Vec::new(),
        blocked: vec![known::BlockedKey {
            key: *ana.identity.public(),
            since: 1_790_000_000,
        }],
    };
    let (log, captured) = Log::capture(64);
    let mut rig = Rig::reopened(start, Arc::new(Identity::generate()), devices, log);
    let bad = rig.host.drops.bad;
    let knock = ana.knock(&mut rig, start);
    assert!(
        ana.answer(knock, start).is_none(),
        "a blocked key was answered"
    );
    assert_eq!(rig.host.drops.bad, bad + 1);
    assert!(rig.host.pending.is_empty());
    let lines = captured.lines();
    assert!(
        lines
            .iter()
            .any(|line| line.ends_with("dropped: the key is blocked")),
        "{lines:?}"
    );

    // Nor does a code sent back, even one answering a live invite the
    // blocked person saw, and no punch goes toward them.
    for answers in [Answers::Invite(rig.invite.invite_id), Answers::Rejoin] {
        let code = code_for(&ana, answers, Some(ana.wire.addr()));
        assert_eq!(
            rig.host.accept_reply(&code, start),
            Err(ReplyRefused::Blocked)
        );
    }
    rig.tick(start);
    assert_eq!(punches(&ana.wire), 0);
}

#[test]
fn known_device_may_send_rejoin_code() {
    let start = Instant::now();
    let ana = Guest::new();
    let devices = KnownDevices {
        devices: vec![known::KnownDevice {
            key: *ana.identity.public(),
            name: String::from("Ana"),
            first_seen: 1_790_000_000,
            last_seen: 1_790_000_000,
            secret: Zeroizing::new([4; 32]),
        }],
        blocked: Vec::new(),
    };
    let mut rig = Rig::reopened(start, Arc::new(Identity::generate()), devices, Log::off());
    let code = code_for(&ana, Answers::Rejoin, Some(SocketAddr::V4(OUTSIDE)));
    let accepted = rig.host.accept_reply(&code, start).expect("accepted");
    assert_eq!(accepted.to, [SocketAddr::V4(OUTSIDE)]);
    // A stranger's key is not known, whatever the code says.
    let bo = Guest::new();
    let code = code_for(&bo, Answers::Rejoin, Some(SocketAddr::V4(OUTSIDE)));
    assert_eq!(
        rig.host.accept_reply(&code, start),
        Err(ReplyRefused::NotKnown)
    );
}

#[test]
fn friends_told_host_addresses() {
    let start = Instant::now();
    let lan = Candidate {
        kind: CandidateKind::Lan,
        addr: "192.0.2.10:41000".parse().unwrap(),
    };
    let devices = DeviceBook::new(KnownDevices::default(), true);
    let identity = Arc::new(Identity::generate());
    let (mut host, socket) = Rig::build(
        start,
        Log::off(),
        Timers::default(),
        false,
        identity,
        devices,
        vec![lan],
    );
    let name = AddressName::new(String::from("myroom.example.net"), Lookup::default());
    host.address_name = Some(name);
    let server = Wire::new();
    host.stun_found(vec![server.addr()], &socket);
    host.stun_resolved(start);
    answer_stun(&mut host, &socket, &server, OUTSIDE, start);
    let mut rig = Rig::holding(host, socket, start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let heard = ana.heard(start);
    assert!(
        matches!(
            heard[..],
            [
                Message::Hello { reached: None, .. },
                Message::PeerSecret { .. },
                ..
            ]
        ),
        "the host's Hello, then the secret: {heard:?}"
    );
    let port = rig.host.port;
    let public = |addr: SocketAddrV4| Candidate {
        kind: CandidateKind::Public,
        addr: SocketAddr::V4(addr),
    };
    let bound = |addr: SocketAddrV4| SocketAddrV4::new(*addr.ip(), port);
    let name = Some(String::from("myroom.example.net"));
    assert_eq!(
        addresses_in(&heard),
        [(
            vec![lan, public(OUTSIDE), public(bound(OUTSIDE))],
            name.clone()
        )]
    );

    // Nothing new, nothing sent.
    let later = start + Duration::from_millis(100);
    rig.tick(later);
    assert!(addresses_in(&ana.heard(later)).is_empty());

    rig.host.address_changed(later, &rig.socket, None);
    answer_stun(&mut rig.host, &rig.socket, &server, MOVED, later);
    assert_eq!(
        addresses_in(&ana.heard(later)),
        [(vec![lan, public(MOVED), public(bound(MOVED))], name)]
    );
}

// Whether a friend's copy reads PC sound, and a pointer update's mark,
// is its latest Hello's version, and a link starts out without either.
#[test]
fn pc_sound_goes_by_each_hello() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let mut now = start + Duration::from_secs(1);
    rig.tick(now);
    for (version, reads) in [
        (
            invite::Version {
                major: 1,
                minor: 2,
                patch: 0,
            },
            false,
        ),
        (peer::PC_SOUND_SINCE, true),
        (
            invite::Version {
                major: 1,
                minor: 2,
                patch: 9,
            },
            false,
        ),
    ] {
        now += Duration::from_millis(10);
        let hello = Message::Hello {
            version,
            name: String::from("Ana"),
            reached: None,
        };
        ana.say(&mut rig, &hello, now);
        rig.tick(now);
        assert_eq!(rig.host.peers[0].link.sound_read, reads, "{version}");
        assert_eq!(rig.host.peers[0].link.marks_read, reads, "{version}");
    }
}

#[test]
fn repeated_hellos_do_not_multiply_rosters() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    let bo = {
        let mut bo = Guest::new();
        ana.join(&mut rig, "Ana", start);
        bo.join(&mut rig, "Bo", start);
        bo
    };
    let mut now = start + Duration::from_secs(1);
    rig.tick(now);
    bo.wire.packets();

    for i in 0..60 {
        now += Duration::from_millis(10);
        let name = if i % 2 == 0 { "Ana" } else { "Anna" };
        let hello = Message::Hello {
            version: invite::VERSION,
            name: name.to_owned(),
            reached: None,
        };
        ana.say(&mut rig, &hello, now);
        rig.tick(now);
    }
    now += ROSTER_GAP;
    rig.tick(now);

    // Sixty renames in 600 ms. What reaches Bo is a few rosters and
    // their retransmits, not one roster per packet Ana sent.
    let reached_bo = bo.wire.packets().len();
    assert!(reached_bo <= 15, "{reached_bo} packets reached Bo");
    assert_eq!(rig.host.peers[0].name, "Anna");
    assert_eq!(rig.host.roster().entries[1].name, "Anna");
}

// Nothing came in after the punches: once the friend has had lost_after
// to answer them, the line under the paste field says so instead of
// going away.
#[test]
fn unanswered_punches_turn_the_paste_to_missed() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let ana = Guest::new();
    let code = code_for(
        &ana,
        Answers::Invite(rig.invite.invite_id),
        Some(ana.wire.addr()),
    );
    rig.host.accept_reply(&code, start).expect("accepted");
    let mut now = start;
    for _ in 0..crate::reply::PUNCH_ROUNDS {
        rig.tick(now);
        now += crate::reply::PUNCH_GAP;
    }
    let missed_at = now - crate::reply::PUNCH_GAP + rig.host.timers.lost_after;
    assert!(rig.host.next_deadline().is_some_and(|at| at <= missed_at));
    rig.tick(missed_at - Duration::from_millis(1));
    assert_eq!(rig.host.view(missed_at, 0).paste, Some(PasteState::Sent));
    rig.tick(missed_at);
    assert_eq!(rig.host.view(missed_at, 0).paste, Some(PasteState::Missed));
    // A forward made meanwhile still lets her in, and the line follows.
    let mut ana = ana;
    ana.join(&mut rig, "Ana", missed_at);
    assert_eq!(rig.host.view(missed_at, 0).paste, Some(PasteState::Joined));
}

// A ninth person's code is refused as it is, not punched for and then
// blamed on the routers.
#[test]
fn a_code_for_a_full_room_is_refused() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    for i in 0..MAX_CLIENTS {
        Guest::new().join(&mut rig, &format!("Friend {i}"), start);
    }
    assert_eq!(rig.host.peers.len(), MAX_CLIENTS);
    let ninth = Guest::new();
    let code = code_for(
        &ninth,
        Answers::Invite(rig.invite.invite_id),
        Some(ninth.wire.addr()),
    );
    assert_eq!(
        rig.host.accept_reply(&code, start),
        Err(ReplyRefused::RoomFull)
    );
    assert_eq!(
        rig.host.view(start, 0).paste,
        Some(PasteState::Refused(ReplyRefused::RoomFull))
    );
    rig.tick(start);
    assert_eq!(punches(&ninth.wire), 0);
}

#[test]
fn the_last_seat_goes_to_the_first_to_confirm() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    for i in 0..MAX_CLIENTS - 1 {
        Guest::new().join(&mut rig, &format!("Friend {i}"), start);
    }
    assert_eq!(rig.host.peers.len(), MAX_CLIENTS - 1);

    // Answers to an invite hold no seat: one with a wrong secret would
    // hold it as well as one with the right one.
    let mut stranger = Guest::new();
    let kind = InitKind::Invite(rig.invite.invite_id);
    let from = stranger.wire.addr();
    stranger.initiate(&mut rig, kind, &[0x5a; 32], from, start);
    let mut first = Guest::new();
    let mut second = Guest::new();
    let a = first.knock(&mut rig, start);
    let b = second.knock(&mut rig, start);
    first.session = Some(first.answer(a, start).expect("first answered"));
    second.session = Some(second.answer(b, start).expect("second answered"));
    let hello = |name: &str| Message::Hello {
        version: invite::VERSION,
        name: name.to_owned(),
        reached: None,
    };
    first.say(&mut rig, &hello("First"), start);
    second.say(&mut rig, &hello("Second"), start);
    assert_eq!(rig.host.peers.len(), MAX_CLIENTS);
    assert!(rig.host.is_peer(first.identity.public()));
    assert!(!rig.host.is_peer(second.identity.public()));
}

#[test]
fn key_flood_keeps_the_rekey() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let secret = rig.host.keys[ana.identity.public()]
        .secret
        .clone()
        .expect("the host made Ana a secret");

    let mut now = start + Duration::from_secs(1);
    let from = ana.wire.addr();
    let rekey = ana.initiate(&mut rig, InitKind::Rekey, &secret, from, now);
    let rekeyed = ana.answer(rekey, now).expect("the rekey is answered");

    // Someone holding the invite fills every seat left, as many tries per
    // key as the host keeps, each from its own address so no one source
    // runs out of tokens. 25 a second keeps the host from going under
    // load, where every one of these would get a cookie reply instead.
    let mut strangers: Vec<Guest> = (0..MAX_CLIENTS - 1).map(|_| Guest::new()).collect();
    let mut source = 0u8;
    for _ in 0..PENDING_PER_KEY {
        for stranger in &mut strangers {
            now += Duration::from_millis(40);
            source += 1;
            let from = SocketAddr::from(([127, 0, 1, source], 9));
            let psk = stranger.invite_psk(&rig.invite);
            let kind = InitKind::Invite(rig.invite.invite_id);
            stranger.initiate(&mut rig, kind, &psk, from, now);
        }
    }
    assert_eq!(rig.host.pending.len(), PENDING_TOTAL);

    ana.session = Some(rekeyed);
    let first = ping(ana.session.as_mut().expect("rekeyed"), 0);
    rig.deliver(&first, from, now);
    assert_eq!(rig.host.peers[0].rekeys, 1, "the rekey was pushed out");
}

fn load_initiations() -> u32 {
    Timers::default().load_initiations
}

fn reads(rig: &Rig) -> u64 {
    rig.host.initiations_read.load(Ordering::Relaxed)
}

// An initiation from someone with the host key and no invite. Under load
// the host looks no further than mac2, so one packet sent again and
// again stands for a flood.
fn stranger_packet(rig: &Rig) -> Vec<u8> {
    let stranger = Identity::generate();
    Initiation::start(
        &stranger.private_bytes(),
        stranger.public(),
        &rig.invite.host_key,
        &[0; 32],
        InitKind::Known,
        Tai64N::now(),
        1,
    )
    .expect("start an initiation")
    .1
}

// Enough initiations to put the host under load, a millisecond apart,
// each from an address of its own. Returns when the last came in.
fn flood(rig: &mut Rig, now: Instant) -> Instant {
    let packet = stranger_packet(rig);
    let mut at = now;
    for n in 0..load_initiations() {
        at = now + Duration::from_millis(u64::from(n));
        let from = SocketAddr::from(([127, 0, 3, 1 + n as u8], 9));
        rig.deliver(&packet, from, at);
    }
    at
}

fn cookie_replies(packets: &[Vec<u8>]) -> usize {
    packets
        .iter()
        .filter(|packet| session::packet_type(packet) == Some(PacketType::CookieReply))
        .count()
}

#[test]
fn no_cookie_reply_without_load() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let packet = stranger_packet(&rig);
    // One under the threshold every second for three seconds.
    let per_second = load_initiations() - 1;
    let mut sent = 0u8;
    for second in 0..3 {
        for k in 0..u64::from(per_second) {
            sent += 1;
            let at = start + Duration::from_secs(second) + Duration::from_millis(k * 32);
            rig.deliver(&packet, SocketAddr::from(([127, 0, 4, sent], 9)), at);
        }
    }
    // A friend joining makes one more within the last second, and that
    // is still one short.
    let now = start + Duration::from_secs(3);
    let mut ana = Guest::new();
    let mut tried = ana.knock(&mut rig, now);
    let packets = ana.wire.packets();
    assert_eq!(cookie_replies(&packets), 0);
    assert!(
        packets
            .iter()
            .any(|packet| tried.finish(packet, now).is_ok())
    );
    assert_eq!(reads(&rig), u64::from(sent) + 1);
    assert_eq!(rig.host.cookies.replies_sent, 0);
    assert_eq!(rig.host.drops.no_cookie, 0);
}

#[test]
fn under_load_no_mac2_gets_cookie() {
    let start = Instant::now();
    let (log, captured) = Log::capture(1024);
    let mut rig = Rig::with_log(start, log);
    let now = flood(&mut rig, start);
    // Every one before the threshold was read. The one that reached it
    // was not.
    let read = reads(&rig);
    assert_eq!(read, u64::from(load_initiations() - 1));

    let mut ana = Guest::new();
    let first = ana.knock(&mut rig, now);
    assert_eq!(reads(&rig), read);
    let packets = ana.wire.packets();
    assert_eq!(packets.len(), 1, "a cookie reply and nothing else");
    assert_eq!(packets[0].len(), COOKIE_REPLY_LEN);
    let (index, cookie) =
        session::read_cookie_reply(&packets[0], &rig.invite.host_key, &first.mac1())
            .expect("the reply opens with the try's mac1");
    assert_eq!(index, first.sender_index());

    let second = ana.knock_with(&mut rig, Some(&cookie), now);
    assert_eq!(reads(&rig), read + 1);
    assert!(
        ana.answer(second, now).is_some(),
        "with the cookie it is answered"
    );

    // The initiation that reached the threshold got one too.
    let numbers = rig.host.view(now, 0).numbers;
    assert_eq!(numbers.cookie_replies, 2);
    assert_eq!(numbers.dropped_no_cookie, 2);
    let lines = captured.lines();
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("under load: 32 initiations within a second")),
        "{lines:?}"
    );
    let heard = format!("from {}, ", ana.wire.addr());
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with(&heard) && line.contains("cookie reply sent")),
        "{lines:?}"
    );
}

// Windows sends nothing to 0.0.0.0, which stands in for a send that
// fails when a flood has used up the socket's buffers.
#[test]
fn unsent_cookie_reply_not_logged_sent() {
    let start = Instant::now();
    let (log, captured) = Log::capture(1024);
    let mut rig = Rig::with_log(start, log);
    let now = flood(&mut rig, start);
    let sent = rig.host.cookies.replies_sent;
    let dropped = rig.host.drops.no_cookie;

    let from = SocketAddr::from((Ipv4Addr::UNSPECIFIED, 9));
    rig.deliver(&stranger_packet(&rig), from, now);
    assert_eq!(rig.host.cookies.replies_sent, sent);
    assert_eq!(rig.host.drops.no_cookie, dropped + 1);
    let heard = format!("from {from}, ");
    let lines: Vec<String> = captured
        .lines()
        .into_iter()
        .filter(|line| line.starts_with(&heard))
        .collect();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].contains("dropped: could not send the cookie reply: "),
        "{lines:?}"
    );
}

#[test]
fn one_source_under_load() {
    let start = Instant::now();
    let (log, captured) = Log::capture(4096);
    let mut rig = Rig::with_log(start, log);
    let now = flood(&mut rig, start);
    let mut ana = Guest::new();
    let heard = format!("from {}, ", ana.wire.addr());
    let cookie_lines = |lines: Vec<String>| {
        lines
            .iter()
            .filter(|line| line.starts_with(&heard) && line.contains("cookie reply sent"))
            .count()
    };

    let tries: Vec<Initiation> = (0..30).map(|_| ana.knock(&mut rig, now)).collect();
    let replies = ana.wire.packets();
    assert_eq!(cookie_replies(&replies), 20, "one source's burst");
    assert_eq!(replies.len(), 20);
    assert_eq!(rig.host.drops.no_cookie, 31);
    assert_eq!(cookie_lines(captured.lines()), 1);

    // Sending the cookie back gets key math again, at one source's rate.
    let (_, cookie) =
        session::read_cookie_reply(&replies[0], &rig.invite.host_key, &tries[0].mac1())
            .expect("the first reply answers the first try");
    let later = now + Duration::from_secs(1);
    let read = reads(&rig);
    for _ in 0..30 {
        ana.knock_with(&mut rig, Some(&cookie), later);
    }
    assert_eq!(reads(&rig), read + 10);

    // A minute on, under load again, the next one has a line of its own.
    let again = flood(&mut rig, now + Duration::from_secs(60));
    ana.knock(&mut rig, again);
    assert_eq!(cookie_lines(captured.lines()), 1);
}

#[test]
fn cookie_replies_spare_key_math() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let now = flood(&mut rig, start);
    // More sources than the shared budget has tokens, all at once. Each
    // is a socket, since Windows stops saying port unreachable for a
    // while after a burst of them, and the mapper's tests running
    // alongside wait for one.
    let packet = stranger_packet(&rig);
    let sources: Vec<Wire> = (1..=60)
        .map(|n| Wire::at(Ipv4Addr::new(127, 0, 5, n)))
        .collect();
    for source in &sources {
        rig.deliver(&packet, source.addr(), now);
    }
    assert_eq!(rig.host.cookies.replies_sent, 61);

    let mut ana = Guest::new();
    let first = ana.knock(&mut rig, now);
    let packets = ana.wire.packets();
    let (_, cookie) = session::read_cookie_reply(&packets[0], &rig.invite.host_key, &first.mac1())
        .expect("a cookie reply");
    let second = ana.knock_with(&mut rig, Some(&cookie), now);
    assert!(ana.answer(second, now).is_some());
}

// The counts in the lines that sum up what the cap held back.
fn capped_counts(lines: Vec<String>) -> Vec<u64> {
    lines
        .iter()
        .filter_map(|line| {
            line.strip_prefix("cookie replies at their cap, ")?
                .strip_suffix(" dropped in the last second")?
                .parse()
                .ok()
        })
        .collect()
}

// A flood from many addresses, each within its own rate, together twice
// what the cap lets out.
#[test]
fn cookie_replies_keep_to_cap() {
    let start = Instant::now();
    let (log, captured) = Log::capture(4096);
    let timers = Timers {
        cookie_replies_per_second: 100,
        cookie_reply_burst: 20,
        ..Timers::default()
    };
    let cap = u64::from(timers.cookie_replies_per_second);
    let mut rig = Rig::with_timers(start, log, timers);
    let now = flood(&mut rig, start);
    let read = reads(&rig);
    let packet = stranger_packet(&rig);
    // Each a socket, as in the test above.
    let sources: Vec<Wire> = (1..=40)
        .map(|n| Wire::at(Ipv4Addr::new(127, 0, 6, n)))
        .collect();
    let sent = |rig: &Rig| rig.host.cookies.replies_sent;

    // All at once, the burst goes out, less the one the last initiation
    // of the flood took.
    let before = sent(&rig);
    for source in &sources {
        rig.deliver(&packet, source.addr(), now);
    }
    let burst = u64::from(timers.cookie_reply_burst);
    assert_eq!(sent(&rig) - before, burst - 1);
    let lines = captured.lines();
    let heard = format!("from {}, ", sources[39].addr());
    assert!(
        lines.iter().any(|line| line.starts_with(&heard)
            && line.contains("dropped: cookie replies at their cap, no cookie reply")),
        "{lines:?}"
    );
    let mut counts = capped_counts(lines);

    // Then one every 5 ms, 5 a second from each source, well within its
    // own rate, for three seconds, with the timer thread's ticks between.
    let mut at = now;
    for second in 0..3 {
        let before = sent(&rig);
        for n in 0..200 {
            at += Duration::from_millis(5);
            rig.deliver(&packet, sources[n % sources.len()].addr(), at);
            rig.tick(at);
        }
        let replies = sent(&rig) - before;
        assert!(
            replies <= cap && replies + 1 >= cap,
            "{replies} cookie replies in second {second}, the cap is {cap} a second"
        );
        counts.extend(capped_counts(captured.lines()));
        assert!(counts.len() <= second + 1, "{counts:?} by second {second}");
    }
    let last = at + Duration::from_secs(1);
    rig.tick(last);
    counts.extend(capped_counts(captured.lines()));

    // Every one is counted with the rest dropped for want of mac2, and
    // every one the cap held back is in a summary line, one a second.
    let initiations = 1 + sources.len() as u64 + 600;
    let numbers = rig.host.view(last, 0).numbers;
    assert_eq!(numbers.dropped_no_cookie, initiations);
    assert_eq!(numbers.cookie_replies, sent(&rig));
    assert_eq!(counts.iter().sum::<u64>(), initiations - sent(&rig));
    assert!(counts.len() <= 4, "{counts:?}");

    // What was sent reached the sources, all but the flood's one.
    let reached: usize = sources
        .iter()
        .map(|source| cookie_replies(&source.packets()))
        .sum();
    assert_eq!(reached as u64, sent(&rig) - 1);
    assert_eq!(reads(&rig), read, "no key math for any of them");
}

#[test]
fn load_ends_after_calm() {
    let start = Instant::now();
    let (log, captured) = Log::capture(1024);
    let mut rig = Rig::with_log(start, log);
    flood(&mut rig, start);
    // They all came in within 31 ms of the first, so the count drops
    // under the threshold a second after the first.
    let over = start + Duration::from_secs(1) + Timers::default().load_calm;
    assert_eq!(rig.host.cookies.next_deadline(), Some(over));
    assert!(rig.host.next_deadline().is_some_and(|at| at <= over));

    let mut ana = Guest::new();
    ana.knock(&mut rig, over - Duration::from_millis(1));
    assert_eq!(cookie_replies(&ana.wire.packets()), 1);

    rig.tick(over);
    let lines = captured.lines();
    assert!(
        lines.iter().any(|line| line
            == "no longer under load: under 32 initiations a second for 5.0 s; 2 cookie replies sent and 2 initiations dropped for want of mac2 so far"),
        "{lines:?}"
    );
    assert_eq!(rig.host.cookies.next_deadline(), None);
    let tried = ana.knock(&mut rig, over);
    assert!(
        ana.answer(tried, over).is_some(),
        "answered as before the load"
    );
}

#[test]
fn full_key_table_makes_way() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let ana = {
        let mut ana = Guest::new();
        ana.join(&mut rig, "Ana", start);
        ana
    };
    for n in 1..MAX_KEYS as u64 {
        let mut key = [7u8; 32];
        key[..8].copy_from_slice(&n.to_le_bytes());
        let mut record = KeyRecord::new(start + Duration::from_millis(n));
        record.secret = Some(new_secret());
        rig.host.keys.insert(key, record);
    }
    assert_eq!(rig.host.keys.len(), MAX_KEYS);

    // Every other record holds a secret, and Ana, seen longest ago, is
    // in the room. The oldest of the rest makes way.
    let later = start + Duration::from_secs(5);
    Guest::new().join(&mut rig, "Bo", later);
    assert_eq!(rig.host.peers.len(), 2);
    assert_eq!(rig.host.keys.len(), MAX_KEYS);
    assert!(rig.host.keys.contains_key(ana.identity.public()));
    let mut oldest = [7u8; 32];
    oldest[..8].copy_from_slice(&1u64.to_le_bytes());
    assert!(!rig.host.keys.contains_key(&oldest));
}

#[test]
fn room_name_defaults_to_the_host_name() {
    let start = Instant::now();
    let rig = Rig::new(start);
    assert_eq!(rig.host.view(start, 0).room_name, "Mara");
    assert_eq!(rig.host.roster().room, "Mara");
}

#[test]
fn a_failed_socket_stops_the_room() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    Guest::new().join(&mut rig, "Ana", start);
    rig.host.socket_failed();
    let view = rig.host.view(start, 0);
    assert_eq!(view.notice, Some(Notice::SocketFailed));
    assert!(view.invite.is_none());
    assert_eq!(view.people.len(), 1);
    assert_eq!(view.strip.state, LinkState::Alone);
    assert_eq!(rig.host.next_deadline(), None);
    assert!(
        !rig.host
            .on_timer(start + Duration::from_secs(1), &rig.socket)
    );
}

#[test]
fn second_bye_copy_is_quiet() {
    let start = Instant::now();
    let (log, captured) = Log::capture(256);
    let mut rig = Rig::with_log(start, log);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let from = ana.wire.addr();

    // What flush_twice sends on leaving: every frame twice.
    ana.reliable.send(&Message::Bye.encode()).expect("queued");
    let frame = ana.reliable.poll_transmit(start, None).expect("a frame");
    let session = ana.session.as_mut().expect("joined");
    let copies: Vec<Vec<u8>> = (0..3)
        .map(|_| seal(session, Channel::Control, &frame))
        .collect();
    let bad = rig.host.drops.bad;
    rig.deliver(&copies[0], from, start);
    assert!(rig.host.peers.is_empty());
    rig.deliver(&copies[1], from, start);
    assert_eq!(rig.host.drops.bad, bad);
    let lines = captured.lines();
    assert!(
        lines.iter().any(|line| line.ends_with(": left, said bye")),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("unknown session")),
        "{lines:?}"
    );

    // Long after, the same index is anyone's guess again.
    rig.deliver(&copies[2], from, start + LEFT_GRACE);
    assert_eq!(rig.host.drops.bad, bad + 1);
    let lines = captured.lines();
    assert!(
        lines
            .iter()
            .any(|line| line.ends_with("data for an unknown session, dropped")),
        "{lines:?}"
    );
}

// A friend's uplink goes while the host's pings still reach them: the
// host lets them go, and their program keeps sending on its session
// until its own lost_after. Those are a friend's packets, not junk.
#[test]
fn late_packets_from_a_lost_friend_are_quiet() {
    let start = Instant::now();
    let (log, captured) = Log::capture(256);
    let mut rig = Rig::with_log(start, log);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let from = ana.wire.addr();
    let lost_after = rig.host.timers.lost_after;
    let lost = start + lost_after;
    rig.tick(lost);
    assert!(rig.host.peers.is_empty());

    let session = ana.session.as_mut().expect("joined");
    let packets: Vec<Vec<u8>> = (0..3).map(|n| ping(session, n)).collect();
    let bad = rig.host.drops.bad;
    rig.deliver(&packets[0], from, lost + Duration::from_secs(1));
    rig.deliver(&packets[1], from, lost + lost_after);
    assert_eq!(rig.host.drops.bad, bad);
    let lines = captured.lines();
    assert!(
        !lines.iter().any(|line| line.contains("unknown session")),
        "{lines:?}"
    );

    // Past what a Bye gets from when they stopped, it is a guess again.
    rig.deliver(&packets[2], from, lost + lost_after + LEFT_GRACE);
    assert_eq!(rig.host.drops.bad, bad + 1);
}

// A friend who rekeyed leaves two sessions behind, and the list stays at
// its cap with both of them in it.
#[test]
fn just_left_stays_at_its_cap_when_a_leaver_rekeyed() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let secret = rig.host.keys[ana.identity.public()]
        .secret
        .clone()
        .expect("the host made Ana a secret");
    let now = start + Duration::from_secs(1);
    let from = ana.wire.addr();
    let rekey = ana.initiate(&mut rig, InitKind::Rekey, &secret, from, now);
    ana.session = Some(ana.answer(rekey, now).expect("the rekey is answered"));
    let first = ping(ana.session.as_mut().expect("rekeyed"), 0);
    rig.deliver(&first, from, now);
    let sessions = &rig.host.peers[0].sessions;
    let indexes: Vec<u32> = [&sessions.current, &sessions.previous]
        .into_iter()
        .flatten()
        .map(|session| session.local_index())
        .collect();
    assert_eq!(indexes.len(), 2);

    rig.host.just_left = (0..JUST_LEFT_KEPT as u32)
        .map(|n| (n, now + LEFT_GRACE))
        .collect();
    ana.say(&mut rig, &Message::Bye, now);
    assert!(rig.host.peers.is_empty());
    assert_eq!(rig.host.just_left.len(), JUST_LEFT_KEPT);
    for index in indexes {
        assert!(rig.host.left_just_now(index, now));
    }
}

fn texts(lines: &[Arc<ChatLine>]) -> Vec<(&str, &str, bool)> {
    lines
        .iter()
        .map(|line| (line.name.as_str(), line.text.as_str(), line.mine))
        .collect()
}

fn said_texts(messages: &[ChatMessage]) -> Vec<(String, String)> {
    messages
        .iter()
        .filter_map(|message| match message {
            ChatMessage::Said { name, text, .. } => Some((name.clone(), text.clone())),
            ChatMessage::Say { .. } => None,
        })
        .collect()
}

fn say(text: &str) -> Vec<u8> {
    say_at(text, 1_790_284_323_456_789)
}

fn say_at(text: &str, sent_at: u64) -> Vec<u8> {
    ChatMessage::Say {
        text: text.to_owned(),
        sent_at,
    }
    .encode()
}

#[test]
fn chat_goes_out_in_host_order() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let mut bo = Guest::new();
    bo.join(&mut rig, "Bo", start);

    rig.host
        .say(String::from("one"), start, &rig.socket)
        .unwrap();
    ana.chat(&mut rig, &say("two"), start);
    rig.host
        .say(String::from("three"), start, &rig.socket)
        .unwrap();

    let heard = |who: &str, text: &str| (who.to_owned(), text.to_owned());
    assert_eq!(
        said_texts(&bo.said(start)),
        [
            heard("Mara", "one"),
            heard("Ana", "two"),
            heard("Mara", "three")
        ]
    );
    // Nobody is handed their own line back.
    assert_eq!(
        said_texts(&ana.said(start)),
        [heard("Mara", "one"), heard("Mara", "three")]
    );
    let view = rig.host.view(start, 0);
    assert_eq!(
        texts(&view.chat),
        [
            ("Mara", "one", true),
            ("Ana", "two", false),
            ("Mara", "three", true)
        ]
    );
    assert_eq!(view.chat[1].author, *ana.identity.public());
}

// A friend's PC is hostile input: bytes that are not text, text over the
// limit, text of nothing but control characters, too many lines, and a
// message only a host sends are all dropped here, logged without their
// text, and handed to nobody.
#[test]
fn bad_say_goes_no_further() {
    let start = Instant::now();
    let (log, captured) = Log::capture(256);
    let mut rig = Rig::with_log(start, log);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let mut bo = Guest::new();
    bo.join(&mut rig, "Bo", start);
    captured.lines();

    let raw = |text: &[u8]| {
        let mut out = vec![1];
        out.extend_from_slice(&(text.len() as u16).to_le_bytes());
        out.extend_from_slice(text);
        out.extend_from_slice(&7u64.to_le_bytes());
        out
    };
    let lines = ["x"; 21].join("\n");
    let from_a_host = ChatMessage::Said {
        author: *ana.identity.public(),
        name: String::from("Ana"),
        text: String::from("I never said this"),
        sent_at_host: None,
        about: false,
    };
    let bad = rig.host.drops.bad;
    for bytes in [
        raw(&[b'o', b'k', 0xC3, 0x28]),
        raw("q".repeat(chat::MAX_TEXT_BYTES + 1).as_bytes()),
        raw("\u{1}\u{7}\u{1b}\t\r\n\u{202E}\u{200B}".as_bytes()),
        raw(lines.as_bytes()),
        from_a_host.encode(),
    ] {
        bo.chat(&mut rig, &bytes, start);
    }
    assert_eq!(rig.host.drops.bad, bad + 5);
    assert!(rig.host.view(start, 0).chat.is_empty());
    assert!(ana.said(start).is_empty());

    let refused: Vec<String> = captured
        .lines()
        .into_iter()
        .filter(|line| line.contains("refused and not handed on"))
        .collect();
    let whys: Vec<&str> = refused
        .iter()
        .map(|line| line.rsplit(": ").next().unwrap_or_default())
        .collect();
    assert_eq!(
        whys,
        [
            "it does not parse",
            "the message is over 900 bytes",
            "nothing is left of the message once cleaned",
            "the message has more than 20 lines",
            "only a host hands messages on",
        ]
    );
    assert!(refused.iter().all(|line| !line.contains("qqq")));

    // The stream itself is fine, and what keeps the rules gets through,
    // cleaned.
    bo.chat(&mut rig, &raw("\u{202E}fine\u{7}".as_bytes()), start);
    assert_eq!(
        said_texts(&ana.said(start)),
        [(String::from("Bo"), String::from("fine"))]
    );
    let lines = captured.lines();
    let line = lines
        .iter()
        .find(|line| line.starts_with("chat: "))
        .expect("a line for the message");
    assert!(
        line.ends_with("wrote 4 bytes, delivery not measured, handed to 1 friend"),
        "{line}"
    );
}

fn said_only(messages: Vec<ChatMessage>) -> Vec<String> {
    messages
        .into_iter()
        .filter_map(|message| match message {
            ChatMessage::Said { text, .. } => Some(text),
            ChatMessage::Say { .. } => None,
        })
        .collect()
}

// A friend's program saying far more than anyone types: past the burst
// the host refuses the rest, and every other friend gets exactly the
// lines the host kept, however slowly they read.
#[test]
fn chat_flood_cut_to_rate() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let mut bo = Guest::new();
    bo.join(&mut rig, "Bo", start);

    let burst = chat::SAY_BURST as usize;
    let per_second = chat::SAYS_PER_SECOND as usize;
    let bad = rig.host.drops.bad;
    let later = start + Duration::from_secs(1);
    for (from, count, now) in [(0, burst + 50, start), (burst + 50, 25, later)] {
        for n in from..from + count {
            ana.chat(&mut rig, &say(&format!("line {n}")), now);
            // The host's acks, so the window never holds a line back.
            ana.read(now);
        }
    }

    let want: Vec<String> = (0..burst)
        .chain(burst + 50..burst + 50 + per_second)
        .map(|n| format!("line {n}"))
        .collect();
    let kept: Vec<String> = rig
        .host
        .view(later, 0)
        .chat
        .iter()
        .map(|line| line.text.clone())
        .collect();
    assert_eq!(kept, want);
    assert_eq!(rig.host.drops.bad, bad + 50 + 25 - per_second as u64);
    assert_eq!(said_only(bo.said_all(&mut rig, later)), want);
}

fn link_of<'a>(rig: &'a mut Rig, key: &[u8; 32]) -> &'a mut Link {
    let peer = rig.host.peers.iter_mut().find(|peer| peer.key == *key);
    &mut peer.expect("in the room").link
}

// The host hands on a friend's send time on its own clock, converted
// with its offset to that friend, and says "about" when that offset came
// over a jittery link. A wrong sign or no conversion at all would put the
// time 5 s out.
#[test]
fn send_time_on_host_clock() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let mut bo = Guest::new();
    bo.join(&mut rig, "Bo", start);
    let key = *ana.identity.public();
    let times = |messages: Vec<ChatMessage>| -> Vec<(Option<u64>, bool)> {
        messages
            .into_iter()
            .filter_map(|message| match message {
                ChatMessage::Said {
                    sent_at_host,
                    about,
                    ..
                } => Some((sent_at_host, about)),
                ChatMessage::Say { .. } => None,
            })
            .collect()
    };
    // Ana's clock runs 5 s ahead of the host's.
    let ahead = 5_000_000;
    let host_now = rig.host.clock.micros(start);
    let sent = host_now + ahead as u64;

    // No offset to Ana yet, so nobody can tell when she sent it.
    ana.chat(&mut rig, &say_at("one", sent), start);
    assert_eq!(times(bo.said(start)), [(None, false)]);

    link_of(&mut rig, &key).offset.push(ClockSample {
        rtt_us: 400,
        offset_us: ahead,
    });
    ana.chat(&mut rig, &say_at("two", sent), start);
    assert_eq!(times(bo.said(start)), [(Some(host_now), false)]);

    // Ana's pings arrive 1 ms and 31 ms after she sent them, by turns.
    let stats = &mut link_of(&mut rig, &key).stats;
    for seq in 0..32u32 {
        let at = u64::from(seq) * 100_000;
        let transit = if seq % 2 == 0 { 1_000 } else { 31_000 };
        stats.peer_ping_received(seq, at, at + transit);
    }
    assert!(stats.snapshot().jitter_ms.is_some_and(|ms| ms > 5.0));
    ana.chat(&mut rig, &say_at("three", sent), start);
    assert_eq!(times(bo.said(start)), [(Some(host_now), true)]);
}

// What a friend says before the Hello on their link waits for it, so it
// goes on under the name the Hello brings and not the fallback.
#[test]
fn chat_waits_for_the_hello_on_its_link() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut bo = Guest::new();
    bo.join(&mut rig, "Bo", start);
    let mut ana = Guest::new();
    let initiation = ana.knock(&mut rig, start);
    ana.session = Some(ana.answer(initiation, start).expect("the host answers"));

    ana.chat(&mut rig, &say("you there?"), start);
    assert!(rig.host.view(start, 0).chat.is_empty());
    assert!(bo.said(start).is_empty());

    let hello = Message::Hello {
        version: invite::VERSION,
        name: String::from("Ana"),
        reached: None,
    };
    ana.say(&mut rig, &hello, start);
    assert_eq!(
        said_texts(&bo.said(start)),
        [(String::from("Ana"), String::from("you there?"))]
    );
    assert_eq!(
        texts(&rig.host.view(start, 0).chat),
        [("Ana", "you there?", false)]
    );
}

fn problems(view: &View) -> Vec<String> {
    view.chat
        .iter()
        .filter(|line| line.kind == LineKind::Problem)
        .map(|line| line.text.clone())
        .collect()
}

// Nothing after the Hello means the same to a friend of another
// protocol. They get a Bye, and this room's chat says once who has which
// version and what to do, however often they come back.
#[test]
fn other_protocol_friend_sent_away() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut bo = Guest::new();
    bo.join(&mut rig, "Bo", start);
    let mut ana = Guest::new();
    // Newer than this build whatever its own version is, so the host
    // is the one told to update.
    let later = Version {
        major: invite::VERSION.major + 1,
        minor: 0,
        patch: 0,
    };
    for round in 0..3 {
        let now = start + Duration::from_secs(round);
        let initiation = ana.knock(&mut rig, now);
        ana.session = Some(ana.answer(initiation, now).expect("the host answers"));
        let hello = Message::OtherHello {
            protocol: invite::PROTOCOL + 1,
            version: later,
            name: String::from("Ana"),
        };
        ana.say(&mut rig, &hello, now);
        assert_eq!(rig.host.peers.len(), 1, "round {round}: only Bo stays");
        let heard = ana.heard(now);
        assert!(
            matches!(heard.first(), Some(Message::Hello { .. })),
            "{heard:?}"
        );
        assert!(matches!(heard.last(), Some(Message::Bye)), "{heard:?}");
        assert_eq!(ana.said(now), [], "a numbered build says it itself");
        ana.left();
    }
    assert_eq!(
        problems(&rig.host.view(start, 0)),
        [format!(
            "Ana has Booth {later} and this room runs {}, so they could not join. Get the same version as Ana from {}.",
            invite::VERSION,
            invite::RELEASES_PAGE
        )]
    );
}

// Exactly what those builds open with. Their own panel shows the Bye as
// the host closing the room, with the line ahead of it in their chat.
#[test]
fn unversioned_friend_sent_away() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    let initiation = ana.knock(&mut rig, start);
    ana.session = Some(ana.answer(initiation, start).expect("the host answers"));
    let hello = Message::UnversionedHello {
        name: String::from("Ana"),
    };
    ana.say(&mut rig, &hello, start);
    assert!(rig.host.peers.is_empty());
    let took = ana.took(start);
    let this = invite::VERSION;
    let why = ChatMessage::Said {
        author: rig.invite.host_key,
        name: rig.host.name.clone(),
        text: format!(
            "This room runs Booth {this} and you have a test build of Booth made before the first release, so you could not join. Get Booth {this} from {}.",
            invite::RELEASES_PAGE
        ),
        sent_at_host: None,
        about: false,
    };
    assert!(
        matches!(
            &took[..],
            [.., Took::Chat(said), Took::Control(Message::Bye)] if *said == why
        ),
        "{took:?}"
    );
    let lines = took.iter().filter(|took| matches!(took, Took::Chat(_)));
    assert_eq!(lines.count(), 1, "{took:?}");
    assert_eq!(
        problems(&rig.host.view(start, 0)),
        [format!(
            "Ana has a test build of Booth made before the first release, so they could not join. Ask them to get Booth {} from {}.",
            invite::VERSION,
            invite::RELEASES_PAGE
        )]
    );
}

#[test]
fn whoever_is_older_gets_the_newer_version() {
    let this = invite::VERSION;
    let page = invite::RELEASES_PAGE;
    let older = Version {
        major: 0,
        minor: 0,
        patch: 9,
    };
    assert_eq!(
        other_version_line("Ana", Some((invite::PROTOCOL + 1, older))),
        format!(
            "Ana has Booth 0.0.9 and this room runs {this}, so they could not join. Ask them to get Booth {this} from {page}."
        )
    );
    // Two test builds of one version name their protocols.
    let protocol = invite::PROTOCOL;
    assert_eq!(
        other_version_line("Ana", Some((protocol + 1, this))),
        format!(
            "Ana has Booth {this} (protocol {}) and this room runs {this} (protocol {protocol}), so they could not join. Get the same version as Ana from {page}.",
            protocol + 1
        )
    );
}

const OUTSIDE: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 7), 41105);

fn pcp_at(external: SocketAddrV4) -> Report {
    Report::Mapped {
        protocol: Protocol::Pcp,
        external,
        lifetime: 7200,
    }
}

fn publics(invite: &Invite) -> Vec<SocketAddr> {
    invite
        .candidates
        .iter()
        .filter(|c| c.kind == CandidateKind::Public)
        .map(|c| c.addr)
        .collect()
}

fn shown(host: &Host, now: Instant) -> InviteView {
    host.view(now, 0).invite.expect("an invite, or its slot")
}

#[test]
fn name_checked_against_stun_or_router() {
    let start = Instant::now();
    let (log, captured) = Log::capture(64);
    let (mut host, _socket) = Rig::host(start, log, true);
    let named = String::from("myroom.example.net");
    host.address_name = Some(AddressName::new(named.clone(), Lookup::default()));
    host.stun_resolved(start);
    host.port_mapping(pcp_at(OUTSIDE), start);
    let outcome = Outcome {
        servers: None,
        result: Ok(net::dns::Resolved {
            addrs: vec![net::dns::Found {
                ip: (*OUTSIDE.ip()).into(),
                source: net::dns::Source::Authoritative,
            }],
            refused: Vec::new(),
        }),
    };
    assert!(host.name_found(outcome));
    let checked = |host: &Host| {
        let name = host.view(start, 0).numbers.address_name.expect("the name");
        assert_eq!(name.name, named);
        name.outside.expect("both are known")
    };
    let router_says = checked(&host);
    assert_eq!(router_says.outside, *OUTSIDE.ip());
    assert!(router_says.is_this_pc());

    // STUN goes by what the internet sees, so it wins over the router.
    let moved = Ipv4Addr::new(198, 51, 100, 4);
    host.stun.set_public_v4(SocketAddrV4::new(moved, 41105));
    host.stun_answered();
    let stun_says = checked(&host);
    assert_eq!(stun_says.points_to, *OUTSIDE.ip());
    assert_eq!(stun_says.outside, moved);
    assert!(!stun_says.is_this_pc());

    let lines: Vec<String> = captured
        .lines()
        .into_iter()
        .filter(|line| line.starts_with("address name"))
        .collect();
    assert_eq!(
        lines,
        [
            "address name myroom.example.net points to 203.0.113.7, this pc's outside address",
            "address name myroom.example.net points to 203.0.113.7, not this pc's outside address 198.51.100.4; friends who look it up cannot reach this pc until the dynamic dns client updates it",
        ]
    );
}

// Behind two routers with STUN blocked, the inner router's outside
// address is a private one, and a name pointing past it is no mistake.
#[test]
fn second_router_address_not_checked() {
    let start = Instant::now();
    let (log, captured) = Log::capture(64);
    let (mut host, _socket) = Rig::host(start, log, true);
    host.address_name = Some(AddressName::new(
        String::from("myroom.example.net"),
        Lookup::default(),
    ));
    host.stun_resolved(start);
    let inner = SocketAddrV4::new(Ipv4Addr::new(192, 168, 0, 10), 41105);
    host.port_mapping(pcp_at(inner), start);
    let outcome = Outcome {
        servers: None,
        result: Ok(net::dns::Resolved {
            addrs: vec![net::dns::Found {
                ip: Ipv4Addr::new(198, 51, 100, 20).into(),
                source: net::dns::Source::Authoritative,
            }],
            refused: Vec::new(),
        }),
    };
    assert!(host.name_found(outcome));
    let name = host.view(start, 0).numbers.address_name.expect("the name");
    assert_eq!(name.outside, None);
    let lines: Vec<String> = captured
        .lines()
        .into_iter()
        .filter(|line| line.starts_with("address name"))
        .collect();
    assert_eq!(
        lines,
        [
            "address name myroom.example.net points to 198.51.100.20; stun and the router have not said what this pc's outside address is"
        ]
    );
}

// Asking the router, which answers `report` right away.
fn mapped_rig(start: Instant, report: Report) -> Rig {
    let (mut host, socket) = Rig::host(start, Log::off(), true);
    host.stun_resolved(start);
    host.port_mapping(report, start);
    Rig::holding(host, socket, start)
}

#[test]
fn first_invite_waits_for_mapping() {
    let start = Instant::now();
    let (mut host, socket) = Rig::host(start, Log::off(), true);
    host.stun_resolved(start);
    let waiting = shown(&host, start);
    assert!(waiting.code.is_empty());
    assert_eq!(waiting.router, RouterState::Testing);
    let wait = Timers::default().first_invite_wait;
    assert_eq!(host.next_deadline(), Some(start + wait));

    let now = start + Duration::from_millis(400);
    assert!(host.port_mapping(pcp_at(OUTSIDE), now));
    let rig = Rig::holding(host, socket, now);
    assert!(rig.invite.mapped);
    assert!(!rig.invite.mapped_verified);
    assert!(!rig.invite.second_router);
    assert_eq!(publics(&rig.invite), [SocketAddr::V4(OUTSIDE)]);
    let view = rig.host.view(now, 0);
    assert_eq!(view.invite.unwrap().router, RouterState::Mapped);
    assert_eq!(view.numbers.mapping_protocol.as_deref(), Some("PCP"));
    assert_eq!(view.numbers.mapped_addr, Some(SocketAddr::V4(OUTSIDE)));
}

#[test]
fn first_invite_skips_slow_router() {
    let start = Instant::now();
    let (log, captured) = Log::capture(64);
    let (mut host, socket) = Rig::host(start, log, true);
    host.stun_resolved(start);
    let wait = Timers::default().first_invite_wait;
    let almost = start + wait - Duration::from_millis(1);
    host.on_timer(almost, &socket);
    assert!(shown(&host, almost).code.is_empty());

    // The timer thread can come a little late.
    let now = start + wait + Duration::from_millis(40);
    assert!(host.on_timer(now, &socket));
    let first = shown(&host, now);
    assert!(!first.code.is_empty());
    assert_eq!(first.router, RouterState::Testing);
    assert!(!Invite::decode(&first.code).unwrap().mapped);
    let lines = captured.lines();
    assert!(
        lines.contains(&String::from(
            "the first invite waits no longer for the port mapping, 3.0 s after the room opened"
        )),
        "{lines:#?}"
    );

    // The message follows the router at once, the code only when the
    // user asks for a new one.
    let later = now + Duration::from_secs(2);
    host.port_mapping(pcp_at(OUTSIDE), later);
    let after = shown(&host, later);
    assert_eq!(after.router, RouterState::Mapped);
    assert_eq!(after.code, first.code);
    // The panel offers New invite beside Copy for this.
    assert!(!first.mapped_since);
    assert!(after.mapped_since);
    host.new_invite(false, later);
    let fresh = Invite::decode(&shown(&host, later).code).unwrap();
    assert!(!shown(&host, later).mapped_since);
    assert!(fresh.mapped);
    assert_eq!(publics(&fresh), [SocketAddr::V4(OUTSIDE)]);
}

// A STUN server name that takes seconds to look up, or to fail, holds up
// the first invite no longer than a slow router does, and what the
// router said in the meantime is in it.
#[test]
fn first_invite_skips_slow_lookup() {
    let start = Instant::now();
    let (log, captured) = Log::capture(64);
    let (mut host, socket) = Rig::host(start, log, true);
    let wait = Timers::default().first_invite_wait;
    assert_eq!(host.next_deadline(), Some(start + wait));
    host.port_mapping(pcp_at(OUTSIDE), start + Duration::from_millis(1));
    let almost = start + wait - Duration::from_millis(1);
    host.on_timer(almost, &socket);
    assert!(shown(&host, almost).code.is_empty());

    let now = start + wait;
    assert!(host.on_timer(now, &socket));
    let first = shown(&host, now);
    assert_eq!(first.router, RouterState::Testing);
    let invite = Invite::decode(&first.code).expect("an invite at 3 s");
    assert!(invite.mapped);
    assert_eq!(publics(&invite), [SocketAddr::V4(OUTSIDE)]);
    let lines = captured.lines();
    assert!(
        lines.contains(&String::from(
            "the first invite waits no longer for stun, 3.0 s after the room opened"
        )),
        "{lines:#?}"
    );

    // The names resolve at last, and the sentence catches up.
    let later = now + Duration::from_secs(5);
    assert!(host.stun_resolved(later));
    assert_eq!(shown(&host, later).router, RouterState::Mapped);
    assert_eq!(shown(&host, later).code, first.code);
}

// STUN's keepalive sees a new outside address long before the router is
// asked again, so the mapper is asked to renew now.
#[test]
fn new_stun_address_renews_mapping() {
    let start = Instant::now();
    let (mut host, _socket) = Rig::host(start, Log::off(), true);
    let (renew, asked) = crossbeam_channel::bounded(1);
    host.renew_mapping_with(renew);
    let first = SocketAddrV4::new(*OUTSIDE.ip(), 52000);
    let moved = SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 4), 52000);
    host.stun.set_public_v4(first);
    host.stun_resolved(start);
    host.port_mapping(pcp_at(OUTSIDE), start);
    host.stun_answered();
    assert!(asked.try_recv().is_err());

    host.stun.set_public_v4(moved);
    host.stun_answered();
    assert_eq!(asked.try_recv(), Ok(()));
    // Not a second router while the router has not been asked.
    let view = host.view(start, 0);
    assert_eq!(view.invite.unwrap().router, RouterState::Mapped);
}

#[test]
fn friend_through_mapping_verifies_it() {
    let start = Instant::now();
    let (log, captured) = Log::capture(256);
    let (mut host, socket) = Rig::host(start, log, true);
    host.stun_resolved(start);
    host.port_mapping(pcp_at(OUTSIDE), start);
    let mut rig = Rig::holding(host, socket, start);

    let mut ana = Guest::new();
    let lan = Some(SocketAddr::from(([192, 168, 1, 20], OUTSIDE.port())));
    ana.join_saying(&mut rig, "Ana", lan, start);
    assert_eq!(shown(&rig.host, start).router, RouterState::Mapped);

    let mut bo = Guest::new();
    rig.host.new_invite(true, start);
    rig.invite = Invite::decode(&shown(&rig.host, start).code).unwrap();
    bo.join_saying(&mut rig, "Bo", Some(SocketAddr::V4(OUTSIDE)), start);
    assert_eq!(shown(&rig.host, start).router, RouterState::MappedVerified);
    let lines = captured.lines();
    assert!(
        lines
            .iter()
            .any(|line| line
                .ends_with("reached this host at 203.0.113.7:41105: the PCP mapping works")),
        "{lines:#?}"
    );

    rig.host.new_invite(false, start);
    let next = Invite::decode(&shown(&rig.host, start).code).unwrap();
    assert!(next.mapped);
    assert!(next.mapped_verified);
}

#[test]
fn second_router_keeps_mapping_out() {
    let start = Instant::now();
    let inner = SocketAddrV4::new(Ipv4Addr::new(192, 168, 0, 20), 41105);
    let rig = mapped_rig(start, pcp_at(inner));
    assert!(rig.invite.second_router);
    assert!(!rig.invite.mapped);
    assert!(publics(&rig.invite).is_empty());
    let view = rig.host.view(start, 0);
    assert_eq!(view.invite.unwrap().router, RouterState::SecondRouter);
    // The stats panel still says what the router answered.
    assert_eq!(view.numbers.mapped_addr, Some(SocketAddr::V4(inner)));

    let none = mapped_rig(start, Report::Unmapped { wan: None });
    assert!(!none.invite.second_router);
    assert!(!none.invite.mapped);
    let view = none.host.view(start, 0);
    // No STUN answer and no mapping: nothing in the invite reaches this PC.
    assert_eq!(view.invite.unwrap().router, RouterState::NoAddress);
    assert_eq!(view.numbers.mapping_protocol, None);
}

fn code_for(guest: &Guest, answers: Answers, at: Option<SocketAddr>) -> ReplyCode {
    ReplyCode {
        answers,
        client_key: *guest.identity.public(),
        outside_v4: at.and_then(|addr| match addr {
            SocketAddr::V4(v4) => Some(v4),
            SocketAddr::V6(_) => None,
        }),
        outside_v6: None,
        mapping: Mapping::Easy,
        expires_at: crate::unix_now() + invite::REPLY_SECS,
    }
}

fn punches(wire: &Wire) -> usize {
    wire.packets()
        .iter()
        .filter(|packet| session::is_punch(packet))
        .count()
}

#[test]
fn pasted_code_sends_ten_punches() {
    let start = Instant::now();
    let (log, captured) = Log::capture(256);
    let mut rig = Rig::with_log(start, log);
    let ana = Guest::new();
    let code = code_for(
        &ana,
        Answers::Invite(rig.invite.invite_id),
        Some(ana.wire.addr()),
    );
    let accepted = rig.host.accept_reply(&code, start).expect("accepted");
    assert_eq!(accepted.to, [ana.wire.addr()]);
    assert_eq!(rig.host.view(start, 0).paste, Some(PasteState::Sent));
    assert_eq!(rig.host.next_deadline(), Some(start));

    let mut now = start;
    let mut rounds = Vec::new();
    for _ in 0..15 {
        rig.tick(now);
        let packets = ana.wire.packets();
        assert!(packets.iter().all(|packet| session::is_punch(packet)));
        rounds.push(packets.len());
        now += crate::reply::PUNCH_GAP;
    }
    assert_eq!(rounds, [[1; 10].as_slice(), &[0; 5]].concat());
    let lines = captured.lines();
    assert!(
        lines.iter().any(|line| line
            == &format!(
                "punch round 10 of 10 for {} sent to {}",
                ana.identity.fingerprint(),
                ana.wire.addr()
            )),
        "{lines:#?}"
    );
}

#[test]
fn the_punches_stop_once_the_friend_is_in() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    let code = code_for(
        &ana,
        Answers::Invite(rig.invite.invite_id),
        Some(ana.wire.addr()),
    );
    rig.host.accept_reply(&code, start).expect("accepted");
    let mut now = start;
    for _ in 0..3 {
        rig.tick(now);
        now += crate::reply::PUNCH_GAP;
    }
    assert_eq!(punches(&ana.wire), 3);
    ana.join(&mut rig, "Ana", now);
    assert_eq!(rig.host.view(now, 0).paste, Some(PasteState::Joined));
    for _ in 0..10 {
        rig.tick(now);
        now += crate::reply::PUNCH_GAP;
    }
    assert_eq!(punches(&ana.wire), 0);
}

#[test]
fn code_refusals() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    let bo = Guest::new();
    let open = Answers::Invite(rig.invite.invite_id);
    let at = Some(bo.wire.addr());
    let refused = |rig: &mut Rig, code: &ReplyCode, now: Instant| {
        let why = rig.host.accept_reply(code, now).expect_err("refused");
        assert_eq!(
            rig.host.view(now, 0).paste,
            Some(PasteState::Refused(why.clone()))
        );
        why
    };

    let mut expired = code_for(&bo, open, at);
    expired.expires_at = crate::unix_now() - 1;
    assert_eq!(refused(&mut rig, &expired, start), ReplyRefused::Expired);

    let unknown = code_for(&bo, Answers::Invite([9; 8]), at);
    assert_eq!(
        refused(&mut rig, &unknown, start),
        ReplyRefused::InviteNotLive
    );

    let rejoin = code_for(&bo, Answers::Rejoin, at);
    assert_eq!(refused(&mut rig, &rejoin, start), ReplyRefused::NotKnown);

    let nowhere = code_for(&bo, open, None);
    assert_eq!(refused(&mut rig, &nowhere, start), ReplyRefused::NoAddress);

    let mut hard = code_for(&bo, open, at);
    hard.mapping = Mapping::Hard;
    assert_eq!(refused(&mut rig, &hard, start), ReplyRefused::FriendHard);

    let good = code_for(&bo, open, at);
    assert!(rig.host.accept_reply(&good, start).is_ok());
    let soon = start + crate::reply::PASTE_GAP - Duration::from_millis(1);
    assert_eq!(refused(&mut rig, &good, soon), ReplyRefused::TooSoon);
    let later = start + crate::reply::PASTE_GAP;
    assert!(rig.host.accept_reply(&good, later).is_ok());

    ana.join(&mut rig, "Ana", later);
    let here = code_for(&ana, open, Some(ana.wire.addr()));
    assert_eq!(
        refused(&mut rig, &here, later),
        ReplyRefused::AlreadyHere {
            name: String::from("Ana")
        }
    );
    // Ana has a secret now, so a rejoin code of hers passes that check.
    let rejoin = code_for(&ana, Answers::Rejoin, Some(ana.wire.addr()));
    assert!(matches!(
        rig.host.accept_reply(&rejoin, later),
        Err(ReplyRefused::AlreadyHere { .. })
    ));

    // The host's own router changes ports: nothing it sends can help.
    rig.host
        .stun
        .set_settled(net::stun::Mapping::Hard, Some(OUTSIDE));
    let fresh = code_for(&Guest::new(), open, at);
    assert_eq!(refused(&mut rig, &fresh, later), ReplyRefused::HostHard);

    rig.host.leave(later, &rig.socket);
    assert_eq!(
        rig.host.accept_reply(&fresh, later),
        Err(ReplyRefused::Closed)
    );
}

#[test]
fn single_use_code_only_for_its_key() {
    let start = Instant::now();
    let (mut host, socket) = Rig::host(start, Log::off(), false);
    host.stun_resolved(start);
    host.new_invite(false, start);
    let mut rig = Rig::holding(host, socket, start);
    let mut ana = Guest::new();
    let bo = Guest::new();
    let id = Answers::Invite(rig.invite.invite_id);

    // Answered is not enough: message 1 says nothing of the secret.
    let initiation = ana.knock(&mut rig, start);
    assert!(ana.answer(initiation, start).is_some());
    let for_bo = code_for(&bo, id, Some(bo.wire.addr()));
    assert!(rig.host.accept_reply(&for_bo, start).is_ok());

    // Confirmed under the new keys: the invite is Ana's now.
    ana.join(&mut rig, "Ana", start);
    assert_eq!(
        rig.host.accept_reply(&for_bo, start),
        Err(ReplyRefused::InviteNotLive)
    );
}

#[test]
fn punched_address_takes_one_key() {
    let start = Instant::now();
    let (log, captured) = Log::capture(256);
    let mut rig = Rig::with_log(start, log);
    let mut ana = Guest::new();
    let mut bo = Guest::new();
    let punched = Wire::new();
    let code = code_for(
        &ana,
        Answers::Invite(rig.invite.invite_id),
        Some(punched.addr()),
    );
    rig.host.accept_reply(&code, start).expect("accepted");
    rig.tick(start);
    punched.packets();

    let psk = bo.invite_psk(&rig.invite);
    let kind = InitKind::Invite(rig.invite.invite_id);
    let bad = rig.host.drops.bad;
    bo.initiate(&mut rig, kind, &psk, punched.addr(), start);
    assert!(punched.packets().is_empty(), "Bo was answered");
    assert_eq!(rig.host.drops.bad, bad + 1);
    let lines = captured.lines();
    let why = format!("was punched open for {} only", ana.identity.fingerprint());
    assert!(lines.iter().any(|line| line.ends_with(&why)), "{lines:#?}");

    // Anywhere else Bo gets the usual rules.
    let initiation = bo.knock(&mut rig, start);
    assert!(bo.answer(initiation, start).is_some());

    let psk = ana.invite_psk(&rig.invite);
    let mut initiation = ana.initiate(&mut rig, kind, &psk, punched.addr(), start);
    let answered = punched
        .packets()
        .iter()
        .any(|packet| initiation.finish(packet, start).is_ok());
    assert!(answered, "Ana was not answered at the punched address");
}

#[test]
fn code_cannot_take_held_address() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let open = Answers::Invite(rig.invite.invite_id);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let at_ana = ana.wire.addr();

    // Someone holding the multi-use invite names Ana's address as theirs.
    let bo = Guest::new();
    assert_eq!(
        rig.host
            .accept_reply(&code_for(&bo, open, Some(at_ana)), start),
        Err(ReplyRefused::AddressTaken { addr: at_ana })
    );
    // Nothing was punched open for Bo, so Ana's rekey from there is taken.
    let secret = rig.host.keys[ana.identity.public()]
        .secret
        .clone()
        .expect("the host made Ana a secret");
    let now = start + Duration::from_secs(1);
    let rekey = ana.initiate(&mut rig, InitKind::Rekey, &secret, at_ana, now);
    assert!(ana.answer(rekey, now).is_some(), "Ana's rekey was dropped");

    // An address punched open for Cy is not handed to Dee as well.
    let cy = Guest::new();
    let wire = Wire::new();
    let punched = wire.addr();
    let for_cy = code_for(&cy, open, Some(punched));
    assert!(rig.host.accept_reply(&for_cy, start).is_ok());
    let for_dee = code_for(&Guest::new(), open, Some(punched));
    assert_eq!(
        rig.host.accept_reply(&for_dee, start),
        Err(ReplyRefused::AddressTaken { addr: punched })
    );
    // Cy's own code for it is taken again once the paste gap is over.
    let later = start + crate::reply::PASTE_GAP;
    assert!(rig.host.accept_reply(&for_cy, later).is_ok());
}

#[test]
fn code_decode_refuses_is_refused() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let ana = Guest::new();
    let open = Answers::Invite(rig.invite.invite_id);
    let refused = |rig: &mut Rig, at: &str| {
        let code = code_for(&ana, open, Some(at.parse().unwrap()));
        match rig.host.accept_reply(&code, start) {
            Err(ReplyRefused::BadCode(invite::BuildError::BadAddress { addr, .. })) => {
                assert_eq!(addr.to_string(), at);
            }
            other => panic!("{at}: {other:?}"),
        }
    };
    refused(&mut rig, "192.168.1.20:52000");
    refused(&mut rig, "10.0.0.7:52000");
    refused(&mut rig, "100.64.0.9:52000");

    // Loopback only on a host set up for tests on one PC.
    rig.host.punch_loopback = false;
    refused(&mut rig, "127.0.0.1:52000");
    rig.host.punch_loopback = true;
    let loopback = code_for(&ana, open, Some(ana.wire.addr()));
    assert!(rig.host.accept_reply(&loopback, start).is_ok());
}

#[test]
fn unsendable_ipv6_left_out() {
    let start = Instant::now();
    let (log, captured) = Log::capture(64);
    let mut rig = Rig::with_log(start, log);
    let open = Answers::Invite(rig.invite.invite_id);
    let v6: SocketAddrV6 = "[2001:db8::7]:52000".parse().unwrap();
    // The rig's host has no addresses of its own, so no IPv6 either.
    let mut only_v6 = code_for(&Guest::new(), open, None);
    only_v6.outside_v6 = Some(v6);
    assert_eq!(
        rig.host.accept_reply(&only_v6, start),
        Err(ReplyRefused::NoAddress)
    );
    let lines = captured.lines();
    let why = format!("reply code: {v6} left out, this pc has no ipv6 address to send from");
    assert!(lines.contains(&why), "{lines:#?}");

    let ana = Guest::new();
    let mut both = code_for(&ana, open, Some(ana.wire.addr()));
    both.outside_v6 = Some(v6);
    let accepted = rig.host.accept_reply(&both, start).expect("accepted");
    assert_eq!(accepted.to, [ana.wire.addr()]);
}

#[test]
fn reliable_line_once_a_minute() {
    let start = Instant::now();
    let (log, captured) = Log::capture(256);
    let mut rig = Rig::with_log(start, log);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let prefix = format!("reliable to {}: ", ana.identity.fingerprint());
    let lines = |captured: &crate::log::Captured| {
        captured
            .lines()
            .into_iter()
            .filter(|line| line.starts_with(&prefix))
            .collect::<Vec<_>>()
    };
    let minute = Duration::from_secs(60);
    // Ana pings as a client does, so she stays in the room.
    let from = ana.wire.addr();
    for second in 1..60 {
        let now = start + Duration::from_secs(second);
        let session = ana.session.as_mut().expect("joined");
        let packet = ping(session, second as u32);
        rig.deliver(&packet, from, now);
        rig.tick(now);
    }
    assert!(lines(&captured).is_empty());
    assert!(
        rig.host
            .next_deadline()
            .is_some_and(|at| at <= start + minute)
    );
    rig.tick(start + minute - Duration::from_millis(1));
    assert!(lines(&captured).is_empty());
    rig.tick(start + minute);
    let written = lines(&captured);
    assert_eq!(written.len(), 1, "{written:#?}");
    assert!(written[0].contains(" messages acked, "), "{}", written[0]);
    rig.tick(start + minute + Duration::from_secs(1));
    assert!(lines(&captured).is_empty());
}

fn answer_stun(host: &mut Host, socket: &Socket, server: &Wire, seen: SocketAddrV4, now: Instant) {
    answer_stun_seeing(host, socket, server, SocketAddr::V4(seen), now);
}

fn answer_stun_seeing(
    host: &mut Host,
    socket: &Socket,
    server: &Wire,
    seen: SocketAddr,
    now: Instant,
) {
    let request = server.packets().pop().expect("a stun request");
    let txid: [u8; 12] = request[8..20].try_into().expect("a transaction id");
    let answer = crate::testing::stun_answer_seeing(&txid, seen);
    host.on_packet(&answer, server.addr(), now, socket);
}

const MOVED: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 4), 41105);

// A host whose first STUN round saw OUTSIDE, with Ana in the room from
// `start`, and then Windows reports an address change and STUN sees
// MOVED. Returns when that was.
fn changed_under_ana(start: Instant, name_set: bool) -> (Rig, Guest, Instant) {
    let (mut host, socket) = Rig::host(start, Log::off(), false);
    if name_set {
        let name = AddressName::new(String::from("myroom.example.net"), Lookup::default());
        host.address_name = Some(name);
    }
    let server = Wire::new();
    host.stun_found(vec![server.addr()], &socket);
    assert!(!host.stun_resolved(start));
    answer_stun(&mut host, &socket, &server, OUTSIDE, start);
    let mut rig = Rig::holding(host, socket, start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    ana.wire.packets();

    let noticed = start + Duration::from_secs(1);
    rig.host.address_changed(noticed, &rig.socket, None);
    answer_stun(&mut rig.host, &rig.socket, &server, MOVED, noticed);
    (rig, ana, noticed)
}

#[test]
fn new_public_address_pings_everyone() {
    let start = Instant::now();
    let (rig, ana, noticed) = changed_under_ana(start, false);
    let view = rig.host.view(noticed, 0);
    assert!(view.invite.expect("the invite").address_changed_since);
    assert_eq!(view.numbers.public_addr, Some(SocketAddr::V4(MOVED)));
    let change = view.numbers.address_change.expect("the change");
    assert!(change.this_pc);
    assert_eq!(
        (change.from, change.to),
        (SocketAddr::V4(OUTSIDE), SocketAddr::V4(MOVED))
    );
    assert_eq!(view.address_changed, None, "nobody has missed anything yet");
    let mut session = ana.session.expect("joined");
    let mut plain = Vec::new();
    let pinged = ana.wire.packets().iter().any(|packet| {
        session.decrypt(packet, &mut plain).is_ok()
            && matches!(
                peer::read_plain(&plain),
                Some(Plain::Ping(PingMessage::Ping { .. }))
            )
    });
    assert!(pinged, "no ping from the new address");
}

// The new address can be a new path to each friend, and the old one's
// floor would make a slower new one read as a queue to the host's own
// share, so the round trips start again.
#[test]
fn new_public_address_starts_the_round_trips_again() {
    let ms = Duration::from_millis;
    let start = Instant::now();
    let (mut host, socket) = Rig::host(start, Log::off(), false);
    let server = Wire::new();
    host.stun_found(vec![server.addr()], &socket);
    assert!(!host.stun_resolved(start));
    answer_stun(&mut host, &socket, &server, OUTSIDE, start);
    let mut rig = Rig::holding(host, socket, start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    // 30 s of the old path at 20 ms.
    let link = &mut rig.host.peers[0].link;
    for tenth in 1..=300 {
        link.timed(start + ms(100) * tenth, ms(20));
    }
    let noticed = start + Duration::from_secs(30);
    rig.host.address_changed(noticed, &rig.socket, None);
    answer_stun(&mut rig.host, &rig.socket, &server, MOVED, noticed);
    let link = &mut rig.host.peers[0].link;
    let mut at = noticed;
    for _ in 0..10 {
        at += ms(100);
        link.timed(at, ms(60));
    }
    let rtt = link.round_trip(at).expect("a round trip");
    assert!((rtt.floor_ms - 60.0).abs() < 0.01, "{rtt:?}");
    assert!(!rtt.risen(), "{rtt:?}");
}

// A round that one server left unanswered ends at stun_wait and shows
// this router is hard, and the panel hears of it then.
#[test]
fn a_round_ending_at_stun_wait_shows_the_new_word() {
    let start = Instant::now();
    let (mut host, socket) = Rig::host(start, Log::off(), false);
    let servers = [
        Wire::new(),
        Wire::at(Ipv4Addr::new(127, 0, 0, 3)),
        Wire::at(Ipv4Addr::new(127, 0, 0, 4)),
    ];
    host.stun_found(servers.iter().map(Wire::addr).collect(), &socket);
    assert!(!host.stun_resolved(start));
    for server in &servers {
        answer_stun(&mut host, &socket, server, OUTSIDE, start);
    }

    let noticed = start + Duration::from_secs(1);
    host.address_changed(noticed, &socket, None);
    let other_port = SocketAddrV4::new(*OUTSIDE.ip(), OUTSIDE.port() + 7);
    answer_stun(&mut host, &socket, &servers[0], OUTSIDE, noticed);
    answer_stun(&mut host, &socket, &servers[1], other_port, noticed);
    assert_eq!(
        host.view(noticed, 0).numbers.mapping,
        Some(MappingWord::Easy)
    );

    let ended = noticed + host.timers.stun_wait;
    assert!(!host.on_timer(ended - Duration::from_millis(1), &socket));
    assert!(
        host.on_timer(ended, &socket),
        "the panel keeps the old word"
    );
    assert_eq!(host.view(ended, 0).numbers.mapping, Some(MappingWord::Hard));
}

// The paste field's line for 2 minutes while friends are reconnecting
// or lost, and the host message for up to 5 minutes while a friend who
// was in the room is lost and not back.
#[test]
fn change_sentences_and_their_times() {
    for name_set in [false, true] {
        let start = Instant::now();
        let timers = Timers::default();
        let (mut rig, _ana, noticed) = changed_under_ana(start, name_set);
        let mut shown = |at: Instant| {
            rig.tick(at);
            rig.host.view(at, 0).address_changed
        };
        let sentence = |codes_cannot_help, friends_lost| {
            Some(AddressChanged {
                codes_cannot_help,
                friends_lost,
                name_set,
            })
        };
        assert_eq!(
            shown(start + timers.reconnecting_after),
            sentence(true, false)
        );
        assert_eq!(shown(start + timers.lost_after), sentence(true, true));
        let two_minutes = noticed + CODES_USELESS_FOR;
        assert_eq!(
            shown(two_minutes - Duration::from_millis(1)),
            sentence(true, true)
        );
        assert_eq!(shown(two_minutes), sentence(false, true));
        let five_minutes = noticed + CHANGE_SHOWN_FOR;
        assert_eq!(
            shown(five_minutes - Duration::from_millis(1)),
            sentence(false, true)
        );
        assert_eq!(shown(five_minutes), None);
    }
}

#[test]
fn change_sentence_goes_when_friend_back() {
    let start = Instant::now();
    let timers = Timers::default();
    let (mut rig, mut ana, _) = changed_under_ana(start, false);
    let lost = start + timers.lost_after;
    rig.tick(lost);
    assert!(
        rig.host
            .view(lost, 0)
            .address_changed
            .is_some_and(|shown| shown.friends_lost)
    );

    // Ana's ladder brings her back with the per-peer secret.
    let secret = rig.host.keys[ana.identity.public()]
        .secret
        .clone()
        .expect("a per-peer secret");
    let back = lost + Duration::from_secs(2);
    let from = ana.wire.addr();
    let initiation = ana.initiate(&mut rig, InitKind::Known, &secret, from, back);
    let mut session = ana.answer(initiation, back).expect("the host answers");
    rig.deliver(&ping(&mut session, 0), from, back);
    let view = rig.host.view(back, 0);
    assert_eq!(view.people.len(), 2);
    assert_eq!(view.address_changed, None);
    let took = view.numbers.reconnect_ms.expect("a reconnect time");
    let silence = numbers::millis(back - start);
    assert!((took - silence).abs() < 1.0, "{took} ms, not {silence} ms");
}

const OUTSIDE_V6: &str = "[2001:db8::7]:41105";
const MOVED_V6: &str = "[2001:db8:1::7]:41105";

// This PC slept 20 s, and nobody in the room was silent for it. Ben was
// heard last 5 s before the sleep, so 10 s after the wake makes his
// lost_after. Cara, heard after the wake on the receive thread before
// the timer pass, counts from then and not past it.
#[test]
fn time_asleep_is_not_silence() {
    let start = Instant::now();
    let (log, captured) = Log::capture(256);
    let mut rig = Rig::with_log(start, log);
    let lost_after = rig.host.timers.lost_after;
    let (mut ben, mut ana, mut cara) = (Guest::new(), Guest::new(), Guest::new());
    ben.join(&mut rig, "Ben", start);
    ana.join(&mut rig, "Ana", start + Duration::from_secs(5));
    let asleep = Duration::from_secs(20);
    let woke = start + Duration::from_secs(25);
    cara.join(&mut rig, "Cara", woke);
    let pass = woke + Duration::from_millis(2);
    rig.host.slept(asleep, pass);
    rig.tick(pass);
    assert_eq!(
        rig.host.view(pass, 0).people.len(),
        4,
        "nobody let go at the wake"
    );
    assert!(
        captured
            .lines()
            .iter()
            .any(|line| line.starts_with("this pc was asleep for 20.0 s;")),
        "the sleep is logged"
    );
    let ben_lost = start + asleep + lost_after;
    rig.tick(ben_lost - Duration::from_millis(1));
    assert_eq!(rig.host.view(ben_lost, 0).people.len(), 4);
    rig.tick(ben_lost);
    assert_eq!(
        rig.host.view(ben_lost, 0).people.len(),
        3,
        "Ben, 15 s awake and silent"
    );
    rig.tick(pass + lost_after - Duration::from_millis(1));
    assert_eq!(
        rig.host.view(pass, 0).people.len(),
        2,
        "Ana went at the wake plus 15 s"
    );
    rig.tick(pass + lost_after);
    assert_eq!(
        rig.host.view(pass, 0).people.len(),
        1,
        "Cara counted from the pass"
    );
}

// A dual-stack host: one STUN server sees it over IPv4, the other over
// IPv6, both at `start`. Then the router drops: Ana, in the room from
// `start`, is let go at lost_after, and Ben joins at 30 s.
fn dual_stack_after_ana_left(start: Instant) -> (Rig, Wire, Wire, Guest) {
    let (mut host, socket) = Rig::host(start, Log::off(), false);
    let (v4, v6) = (Wire::new(), Wire::new());
    host.stun_found(vec![v4.addr(), v6.addr()], &socket);
    assert!(!host.stun_resolved(start));
    answer_stun(&mut host, &socket, &v4, OUTSIDE, start);
    let seen_v6 = OUTSIDE_V6.parse().unwrap();
    answer_stun_seeing(&mut host, &socket, &v6, seen_v6, start);
    host.new_invite(true, start);
    let mut rig = Rig::holding(host, socket, start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let lost = start + Timers::default().lost_after;
    rig.tick(lost);
    assert_eq!(rig.host.view(lost, 0).people.len(), 1, "Ana was let go");
    let mut ben = Guest::new();
    ben.join(&mut rig, "Ben", start + Duration::from_secs(30));
    ben.wire.packets();
    (rig, v4, v6, ben)
}

// The router comes back with a new IPv4 address and the same IPv6
// prefix, and the IPv6 answer comes in first. It says nothing about
// when IPv4 was last seen as it was.
#[test]
fn lost_friend_counts_in_either_family() {
    let start = Instant::now();
    let (mut rig, v4, v6, _ben) = dual_stack_after_ana_left(start);
    let back = start + Duration::from_secs(40);
    rig.host.address_changed(back, &rig.socket, None);
    let same_v6 = OUTSIDE_V6.parse().unwrap();
    answer_stun_seeing(&mut rig.host, &rig.socket, &v6, same_v6, back);
    answer_stun(&mut rig.host, &rig.socket, &v4, MOVED, back);
    let shown = rig.host.view(back, 0).address_changed;
    assert!(
        shown.is_some_and(|shown| shown.friends_lost),
        "Ana went with it: {shown:?}"
    );
}

// Both families come back changed, one answer after the other in the
// same round: one change, one ping to each friend, and the IPv4 pair in
// the stats panel.
#[test]
fn both_families_are_one_change() {
    let start = Instant::now();
    let (mut rig, v4, v6, mut ben) = dual_stack_after_ana_left(start);
    let back = start + Duration::from_secs(40);
    rig.host.address_changed(back, &rig.socket, None);
    answer_stun(&mut rig.host, &rig.socket, &v4, MOVED, back);
    let later = back + Duration::from_millis(30);
    let moved_v6 = MOVED_V6.parse().unwrap();
    answer_stun_seeing(&mut rig.host, &rig.socket, &v6, moved_v6, later);

    let view = rig.host.view(later, 0);
    assert!(
        view.address_changed.is_some_and(|shown| shown.friends_lost),
        "Ana went with it: {:?}",
        view.address_changed
    );
    let change = view.numbers.address_change.expect("the change");
    assert_eq!(
        (change.from, change.to),
        (SocketAddr::V4(OUTSIDE), SocketAddr::V4(MOVED))
    );
    assert_eq!(rig.host.change.as_ref().map(|change| change.at), Some(back));
    let session = ben.session.as_mut().expect("joined");
    let mut plain = Vec::new();
    let pings = ben
        .wire
        .packets()
        .iter()
        .filter(|packet| {
            session.decrypt(packet, &mut plain).is_ok()
                && matches!(
                    peer::read_plain(&plain),
                    Some(Plain::Ping(PingMessage::Ping { .. }))
                )
        })
        .count();
    assert_eq!(pings, 1, "one ping from the new address");
}

// After a router restart this PC can have another address toward it,
// and a mapping made for the old one would lead nowhere.
#[test]
fn remap_uses_new_local_address() {
    let start = Instant::now();
    let (log, captured) = Log::capture(256);
    let (mut host, socket) = Rig::host(start, log, true);
    let (remap, asked) = crossbeam_channel::bounded(1);
    host.remap_with(remap);
    let server = Wire::new();
    host.stun_found(vec![server.addr()], &socket);
    assert!(!host.stun_resolved(start));
    answer_stun(&mut host, &socket, &server, OUTSIDE, start);

    let router = Ipv4Addr::new(192, 168, 1, 1);
    let now_at = Ipv4Addr::new(192, 168, 1, 23);
    let listing = Listing {
        addrs: vec![LocalAddr {
            ip: now_at.into(),
            kind: net::addrs::AddrKind::Lan,
            adapter: String::from("Ethernet"),
            has_gateway: true,
            gateway: Some(router.into()),
            vpn_adapter: false,
            hardware_adapter: true,
        }],
        router: Ok(Gateway {
            ip: router,
            local: now_at,
        }),
    };
    let noticed = start + Duration::from_secs(1);
    host.address_changed(noticed, &socket, Some(Ok(listing)));
    answer_stun(&mut host, &socket, &server, MOVED, noticed);
    assert_eq!(
        asked.try_recv(),
        Ok(Gateway {
            ip: router,
            local: now_at
        })
    );
    let lines = captured.lines();
    assert!(
        lines.contains(&String::from(
            "port mapping: the router to ask is now 192.168.1.1, this pc is 192.168.1.23 to it; the next mapping is asked of it"
        )),
        "{lines:#?}"
    );

    // The mapper stops when the router opened no port at the start.
    drop(asked);
    let again = noticed + Duration::from_secs(1);
    host.address_changed(again, &socket, None);
    answer_stun(&mut host, &socket, &server, OUTSIDE, again);
    assert!(captured.lines().contains(&String::from(
        "port mapping: not made again, the mapper stopped after the router opened no port at the start"
    )));
}

// What `session` can open of `packets`, as plain frames.
fn opened(session: &mut Session, packets: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for packet in packets {
        let mut plain = Vec::new();
        if session.decrypt(&packet, &mut plain).is_ok() {
            out.push(plain);
        }
    }
    out
}

fn voice_count(plain: &[Vec<u8>]) -> usize {
    plain
        .iter()
        .filter(|plain| matches!(peer::read_plain(plain), Some(Plain::Voice(_))))
        .count()
}

// Someone on the path who sees Ana's packets sends a copy of a fresh one
// first, from an address of their own. Control follows it at once, and
// Ana's next packet takes it back; the voice relayed to her never goes
// there. A real move takes the voice once a ping to the new address is
// answered from it.
#[test]
fn voice_moves_on_answered_ping() {
    use voice::codec::{Encoder, Mode};

    let mut rig = Rig::new(Instant::now());
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", Instant::now());
    let mut bo = Guest::new();
    bo.join(&mut rig, "Bo", Instant::now());
    let ana_key = *ana.identity.public();
    let home = ana.wire.addr();
    let copier = Wire::new();
    let moved = Wire::new();

    let mut encoder = Encoder::new(Mode::LowDelay, true).unwrap();
    let pcm: Vec<f32> = (0..240).map(|n| 0.2 * (n as f32 * 0.06).sin()).collect();
    let mut opus = [0u8; 20];
    let len = encoder.encode(&pcm, &mut opus).unwrap();
    let mut seq = 0u16;
    let mut bo_talks = |rig: &mut Rig, bo: &mut Guest| {
        let frame = talk::Frame {
            seq,
            captured: 0,
            mode: Mode::LowDelay,
            redundancy: false,
            last: false,
            frame: &opus[..len],
            previous: None,
            pad: 0,
        };
        seq = seq.wrapping_add(1);
        let mut payload = Vec::new();
        frame.write_spoken(&mut payload);
        let packet = seal(bo.session.as_mut().unwrap(), Channel::Voice, &payload);
        rig.deliver(&packet, bo.wire.addr(), Instant::now());
    };
    let ana_peer = |rig: &Rig| {
        let peer = rig.host.peers.iter().find(|p| p.key == ana_key).unwrap();
        (peer.addr, peer.media.to())
    };

    bo_talks(&mut rig, &mut bo);
    let got = opened(ana.session.as_mut().unwrap(), ana.wire.packets());
    assert_eq!(voice_count(&got), 1);

    // The copy wins the race, and the original is a replay.
    let fresh = ping(ana.session.as_mut().unwrap(), 7);
    rig.deliver(&fresh, copier.addr(), Instant::now());
    rig.deliver(&fresh, home, Instant::now());
    assert_eq!(ana_peer(&rig), (copier.addr(), home));
    rig.tick(Instant::now());
    bo_talks(&mut rig, &mut bo);
    let at_copier = opened(ana.session.as_mut().unwrap(), copier.packets());
    assert!(
        at_copier.iter().any(|plain| matches!(
            peer::read_plain(plain),
            Some(Plain::Ping(PingMessage::Ping { .. }))
        )),
        "the new address is pinged at once"
    );
    assert_eq!(voice_count(&at_copier), 0, "voice went to the copier");
    assert_eq!(
        voice_count(&opened(ana.session.as_mut().unwrap(), ana.wire.packets())),
        1
    );

    // Ana's next packet brings control back, and nothing is left to check.
    let next = ping(ana.session.as_mut().unwrap(), 8);
    rig.deliver(&next, home, Instant::now());
    assert_eq!(ana_peer(&rig), (home, home));

    // A real move: voice stays until the ping to the new address comes
    // back from there.
    let from_there = ping(ana.session.as_mut().unwrap(), 9);
    rig.deliver(&from_there, moved.addr(), Instant::now());
    rig.tick(Instant::now());
    bo_talks(&mut rig, &mut bo);
    assert_eq!(
        voice_count(&opened(ana.session.as_mut().unwrap(), ana.wire.packets())),
        1
    );
    let (seq, t1) = opened(ana.session.as_mut().unwrap(), moved.packets())
        .iter()
        .find_map(|plain| match peer::read_plain(plain) {
            Some(Plain::Ping(PingMessage::Ping { seq, t1 })) => Some((seq, t1)),
            _ => None,
        })
        .expect("a ping to the new address");
    let mut pong = Vec::new();
    PingMessage::Pong {
        seq,
        t1,
        t2: t1,
        t3: t1,
    }
    .encode(&mut pong);
    let pong = seal(ana.session.as_mut().unwrap(), Channel::Ping, &pong);
    rig.deliver(&pong, moved.addr(), Instant::now());
    assert_eq!(ana_peer(&rig), (moved.addr(), moved.addr()));
    bo_talks(&mut rig, &mut bo);
    assert_eq!(
        voice_count(&opened(ana.session.as_mut().unwrap(), moved.packets())),
        1
    );
    assert_eq!(
        voice_count(&opened(ana.session.as_mut().unwrap(), ana.wire.packets())),
        0
    );
}

// Ana asks to share and the host grants it.
fn granted(rig: &mut Rig, ana: &mut Guest, now: Instant) -> u32 {
    ana.say(rig, &Message::ShareStart { fps: 120 }, now);
    ana.heard(now)
        .into_iter()
        .find_map(|message| match message {
            Message::ShareAnswer(control::ShareAnswer::Granted { share }) => Some(share),
            _ => None,
        })
        .expect("the share granted")
}

// Each run of the host starts its share numbers at random and counts up
// from there, past u32::MAX to 1 and never to 0.
#[test]
fn a_run_numbers_its_shares_from_a_random_start() {
    let start = Instant::now();
    let mut earlier = Rig::new(start);
    let mut ana = Guest::new();
    ana.join(&mut earlier, "Ana", start);
    let first = granted(&mut earlier, &mut ana, start);
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let share = granted(&mut rig, &mut ana, start);
    assert_ne!(share, first);

    ana.say(&mut rig, &Message::ShareStop, start);
    assert_eq!(
        granted(&mut rig, &mut ana, start),
        share.wrapping_add(1).max(1)
    );

    // Later, so these two asks fit the limit on asks to share whatever
    // its burst.
    let later = start + Duration::from_secs(4);
    rig.host.next_share = u32::MAX;
    ana.say(&mut rig, &Message::ShareStop, later);
    assert_eq!(granted(&mut rig, &mut ana, later), u32::MAX);
    ana.say(&mut rig, &Message::ShareStop, later);
    assert_eq!(granted(&mut rig, &mut ana, later), 1);
}

// A frame of one data and one parity shard of the smallest size, the
// cheapest packet a sharer can send.
fn tiny_packet() -> Vec<u8> {
    let facts = channels::video::FrameFacts::default();
    let payload = channels::video::HEADER + channels::video::MIN_SHARD;
    let mut packetizer = channels::video::Packetizer::new(payload).expect("a packetizer");
    let packets = packetizer.packetize(&facts, &[7], 20).expect("packetized");
    packets.iter().next().expect("a packet").to_vec()
}

fn sent_video(guest: &mut Guest, packet: &[u8]) -> Vec<u8> {
    let mut payload = vec![1, 0];
    payload.extend_from_slice(packet);
    seal(
        guest.session.as_mut().expect("joined"),
        Channel::Video,
        &payload,
    )
}

fn facts_in(messages: &[Message]) -> Vec<control::Facts> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::ShareFacts(facts) => Some(*facts),
            _ => None,
        })
        .collect()
}

fn peer_at(rig: &mut Rig, guest: &Guest) -> usize {
    let key = *guest.identity.public();
    rig.host
        .peers
        .iter()
        .position(|peer| peer.key == key)
        .expect("in the room")
}

// A friend's PC can send as fast as its link goes. With a clock that
// stands still, a sharer gets its burst of 3072 packets, the largest
// frame there can be, then 20 000 a second; the rest is dropped and
// counted, and nothing of it counts as bad.
#[test]
fn sharer_video_burst_and_rate() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let mut bo = Guest::new();
    bo.join(&mut rig, "Bo", start);
    let share = granted(&mut rig, &mut ana, start);
    bo.say(
        &mut rig,
        &Message::Watch {
            share,
            on: true,
            hevc: true,
        },
        start,
    );
    let tiny = tiny_packet();
    let from = ana.wire.addr();
    for _ in 0..3100 {
        let packet = sent_video(&mut ana, &tiny);
        rig.deliver(&packet, from, start);
    }
    assert_eq!(
        (rig.host.screen.relayed, rig.host.screen.dropped),
        (3072, 28)
    );
    let later = start + Duration::from_millis(100);
    for _ in 0..2500 {
        let packet = sent_video(&mut ana, &tiny);
        rig.deliver(&packet, from, later);
    }
    println!(
        "5600 packets, 3100 at once and 2500 a tenth of a second later: {} passed on, {} dropped",
        rig.host.screen.relayed, rig.host.screen.dropped
    );
    assert_eq!(
        (rig.host.screen.relayed, rig.host.screen.dropped),
        (3072 + 2000, 28 + 500)
    );
    assert_eq!(rig.host.drops.bad, 0);
}

// The host's upload setting divided by the watchers on an internet path,
// LAN watchers and people not watching not counted, and 1400-byte
// datagrams only when every link involved is on the LAN. Every link here
// is loopback, so the path words are set by hand.
#[test]
fn upload_split_among_internet_watchers() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut guests: Vec<Guest> = ["Ana", "Bo", "Cy", "Dee"]
        .iter()
        .map(|name| {
            let mut guest = Guest::new();
            guest.join(&mut rig, name, start);
            guest
        })
        .collect();
    for internet in [1, 3] {
        let at = peer_at(&mut rig, &guests[internet]);
        rig.host.peers[at].path = PathWord::Direct;
    }
    let share = {
        let (ana, _) = guests.split_first_mut().unwrap();
        granted(&mut rig, ana, start)
    };
    // Each press a FACTS_EVERY after the one before, so each is told at
    // once.
    let mut at = start;
    let mut told = |rig: &mut Rig, guests: &mut [Guest], who: usize, on: bool| {
        at += crate::screen::FACTS_EVERY;
        let watch = Message::Watch {
            share,
            on,
            hevc: true,
        };
        guests[who].say(rig, &watch, at);
        let facts = facts_in(&guests[0].heard(at));
        *facts.last().expect("the sharer told")
    };
    // A watcher who starts has its IDR ask in them.
    let facts = |watchers, internet, cap_kbps, lan, idr| control::Facts {
        share,
        watchers,
        internet,
        cap_kbps,
        lan,
        hevc: true,
        idr,
    };
    assert_eq!(
        told(&mut rig, &mut guests, 1, true),
        facts(1, 1, 15_000, false, true)
    );
    assert_eq!(
        told(&mut rig, &mut guests, 2, true),
        facts(2, 1, 15_000, false, true)
    );
    assert_eq!(
        told(&mut rig, &mut guests, 3, true),
        facts(3, 2, 7_500, false, true)
    );
    assert_eq!(
        told(&mut rig, &mut guests, 1, false),
        facts(2, 1, 15_000, false, false)
    );
    assert_eq!(
        told(&mut rig, &mut guests, 3, false),
        facts(1, 0, 0, true, false)
    );
    // The sharer's own link counts too.
    let ana = peer_at(&mut rig, &guests[0]);
    rig.host.peers[ana].path = PathWord::Direct;
    let mut at = at + crate::screen::FACTS_EVERY;
    rig.tick(at);
    let facts_now = facts_in(&guests[0].heard(at));
    assert_eq!(facts_now, [facts(1, 0, 0, false, false)]);

    // The host's own share follows the same rule, and the rate its
    // encoder may use is never more than its own setting.
    guests[0].say(&mut rig, &Message::ShareStop, at);
    rig.host.share(120, at, &rig.socket);
    let own = rig
        .host
        .live
        .as_ref()
        .map(|live| live.number)
        .expect("the host shares");
    for who in [1, 3] {
        at += crate::screen::FACTS_EVERY;
        guests[who].say(
            &mut rig,
            &Message::Watch {
                share: own,
                on: true,
                hevc: true,
            },
            at,
        );
    }
    let facts = rig.host.screen.sharing.facts().expect("facts");
    println!("the host's own share with two watchers over the internet: {facts:?}");
    assert_eq!(
        (
            facts.watchers,
            facts.internet,
            facts.rate_kbps,
            facts.spread,
            facts.lan
        ),
        (2, 2, 7_500, true, false)
    );
}

// The largest pointer shape is 256 KB, sent to every watcher over the
// control channel. The first goes; a second right behind it is past the
// bytes a sharer may send, and none of its chunks go on.
#[test]
fn one_largest_shape_at_once() {
    use crate::screen::wire;
    use crate::screen::{MAX_SHAPE_BYTES, Shape, ShapeKind};

    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let mut bo = Guest::new();
    bo.join(&mut rig, "Bo", start);
    let share = granted(&mut rig, &mut ana, start);
    bo.say(
        &mut rig,
        &Message::Watch {
            share,
            on: true,
            hevc: true,
        },
        start,
    );
    bo.heard(start);
    let largest = Shape {
        kind: ShapeKind::Color,
        width: 256,
        height: 256,
        pitch: 1024,
        hotspot_x: 0,
        hotspot_y: 0,
        scale_milli: 1000,
        bytes: vec![1; MAX_SHAPE_BYTES],
    };
    // The host's acks are read now and then, so the window never holds
    // the chunks back.
    let pump = |rig: &mut Rig, ana: &mut Guest| {
        ana.read(start);
        while let Some(frame) = ana.reliable.poll_transmit(start, None) {
            let packet = seal(ana.session.as_mut().unwrap(), Channel::Control, &frame);
            rig.deliver(&packet, ana.wire.addr(), start);
        }
    };
    for id in [1, 2] {
        for (n, chunk) in wire::chunks(share, id, &largest).enumerate() {
            let message = Message::Shape(chunk).encode();
            ana.reliable.send(&message).expect("queued");
            if n % 32 == 31 {
                pump(&mut rig, &mut ana);
            }
        }
    }
    let chunks = MAX_SHAPE_BYTES / crate::screen::SHAPE_CHUNK;
    for _ in 0..8 {
        pump(&mut rig, &mut ana);
    }
    assert_eq!(rig.host.screen.dropped, chunks as u64);
    assert_eq!(rig.host.drops.bad, 0);
    println!(
        "two shapes of {MAX_SHAPE_BYTES} bytes, {chunks} chunks each, one right after the other: the second's {} chunks were held back",
        rig.host.screen.dropped
    );
}

fn shape_chunks(messages: Vec<Message>) -> Vec<crate::screen::wire::ShapeChunk> {
    messages
        .into_iter()
        .filter_map(|message| match message {
            Message::Shape(chunk) => Some(chunk),
            _ => None,
        })
        .collect()
}

// A friend's PC is hostile input. Each chunk is read against the total
// it carries itself, so a sharer could pay for a 4-byte shape with its
// first chunk and send the rest as parts of a 256 KB one. Those go
// nowhere and count as bad; a shape sent by the rules still goes.
#[test]
fn shape_held_to_first_chunk_total() {
    use crate::screen::wire::{self, ShapeChunk};
    use crate::screen::{MAX_SHAPE_BYTES, SHAPE_CHUNK, Shape, ShapeKind};

    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let mut bo = Guest::new();
    bo.join(&mut rig, "Bo", start);
    let share = granted(&mut rig, &mut ana, start);
    bo.say(
        &mut rig,
        &Message::Watch {
            share,
            on: true,
            hevc: true,
        },
        start,
    );
    bo.heard(start);
    let color = |width: u16, height: u16| Shape {
        kind: ShapeKind::Color,
        width,
        height,
        pitch: width * 4,
        hotspot_x: 0,
        hotspot_y: 0,
        scale_milli: 1000,
        bytes: vec![1; usize::from(width) * usize::from(height) * 4],
    };
    let tiny = wire::chunks(share, 1, &color(1, 1))
        .next()
        .expect("a chunk");
    ana.say(&mut rig, &Message::Shape(tiny), start);
    for index in 1..4u16 {
        let bigger = ShapeChunk {
            share,
            id: 1,
            total: MAX_SHAPE_BYTES as u32,
            index,
            head: None,
            bytes: vec![7; SHAPE_CHUNK],
        };
        ana.say(&mut rig, &Message::Shape(bigger), start);
    }
    let passed = shape_chunks(bo.heard(start));
    assert_eq!(passed.len(), 1, "only the 4-byte shape's one chunk");
    assert_eq!(passed[0].total, 4);
    assert_eq!(rig.host.drops.bad, 3);

    let two = color(32, 16);
    for chunk in wire::chunks(share, 2, &two) {
        ana.say(&mut rig, &Message::Shape(chunk), start);
    }
    let passed = shape_chunks(bo.heard(start));
    assert_eq!(passed.len(), 2);
    assert!(
        passed
            .iter()
            .all(|chunk| chunk.id == 2 && chunk.total == 2048)
    );
    assert_eq!(rig.host.drops.bad, 3);
}

// One frame's packets as the packetizer makes them, frame `number`.
fn frame_packet(number: u32) -> Vec<u8> {
    let facts = channels::video::FrameFacts {
        number,
        ..channels::video::FrameFacts::default()
    };
    let payload = channels::video::HEADER + channels::video::MIN_SHARD;
    let mut packetizer = channels::video::Packetizer::new(payload).expect("a packetizer");
    let packets = packetizer.packetize(&facts, &[7], 20).expect("packetized");
    packets.iter().next().expect("a packet").to_vec()
}

fn recovers_in(messages: &[Message]) -> Vec<(u32, u32)> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::Recover { first, last, .. } => Some((*first, *last)),
            _ => None,
        })
        .collect()
}

// A recover request can only be about a frame the host has passed on:
// NVENC answers one about a frame it has not made yet with an IDR, so
// a watcher naming frames ahead could have one every frame.
#[test]
fn recover_for_unsent_frame_dropped() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let mut bo = Guest::new();
    bo.join(&mut rig, "Bo", start);
    let share = granted(&mut rig, &mut ana, start);
    bo.say(
        &mut rig,
        &Message::Watch {
            share,
            on: true,
            hevc: true,
        },
        start,
    );
    ana.heard(start);
    let recover = |first, last| Message::Recover { share, first, last };

    bo.say(&mut rig, &recover(0, 0), start);
    assert!(recovers_in(&ana.heard(start)).is_empty());
    assert_eq!(rig.host.drops.bad, 1);

    let from = ana.wire.addr();
    for number in 0..=5 {
        let packet = sent_video(&mut ana, &frame_packet(number));
        rig.deliver(&packet, from, start);
    }
    bo.say(&mut rig, &recover(4, 5), start);
    bo.say(&mut rig, &recover(5, 6), start);
    assert_eq!(recovers_in(&ana.heard(start)), [(4, 5)]);
    assert_eq!(rig.host.drops.bad, 2);

    // The host's own share: what its own threads sent.
    ana.say(&mut rig, &Message::ShareStop, start);
    rig.host.share(120, start, &rig.socket);
    let own = rig
        .host
        .live
        .as_ref()
        .map(|live| live.number)
        .expect("sharing");
    bo.say(
        &mut rig,
        &Message::Watch {
            share: own,
            on: true,
            hevc: true,
        },
        start,
    );
    rig.host.screen.sharing.take_answers();
    let mut outbox = rig.host.screen.sharing.outbox();
    outbox.video(&frame_packet(40));
    for (first, last) in [(41, 41), (39, 40)] {
        bo.say(
            &mut rig,
            &Message::Recover {
                share: own,
                first,
                last,
            },
            start,
        );
    }
    assert_eq!(
        rig.host.screen.sharing.take_answers(),
        [crate::screen::Answer::Recover {
            first: 39,
            last: 40
        }]
    );
    assert_eq!(rig.host.drops.bad, 3);
}

fn messages(stream: &mut Reliable) -> Vec<Message> {
    let mut out = Vec::new();
    while let Some(bytes) = stream.next_delivered() {
        out.extend(Message::decode(&bytes));
    }
    out
}

// Limits go to a sharer whose Hello says LIMITS_SINCE or later, never to
// an older one, and a watcher's 4K and caps fold into them.
#[test]
fn limits_go_only_to_a_sharer_that_reads_them() {
    let older = Version {
        major: 1,
        minor: 1,
        patch: 0,
    };
    for (version, reads) in [(older, false), (peer::LIMITS_SINCE, true)] {
        let start = Instant::now();
        let mut rig = Rig::new(start);
        rig.host.version = peer::LIMITS_SINCE;
        let mut ana = Guest::new();
        ana.join_as(&mut rig, "Ana", version, None, start);
        let mut bo = Guest::new();
        bo.join_as(&mut rig, "Bo", peer::LIMITS_SINCE, None, start);
        let share = granted(&mut rig, &mut ana, start);
        bo.say(
            &mut rig,
            &Message::Watch {
                share,
                on: true,
                hevc: true,
            },
            start,
        );
        let limits = control::WatchLimits {
            takes_4k: true,
            fps: Some(60),
            ..control::WatchLimits::default()
        };
        bo.say(&mut rig, &Message::WatchLimits { share, limits }, start);
        ana.read(start);
        let told: Vec<_> = messages(&mut ana.reliable)
            .into_iter()
            .filter_map(|message| match message {
                Message::ShareLimits { limits, .. } => Some(limits),
                _ => None,
            })
            .collect();
        let four_k = control::ShareLimits {
            width: 4096,
            height: 2160,
            fps: 60,
            kbps: None,
        };
        if reads {
            assert_eq!(told.last(), Some(&Some(four_k)), "{version}");
        } else {
            assert!(told.is_empty(), "{version}: {told:?}");
        }
        assert_eq!(rig.host.drops.bad, 0, "{version}");
    }
}

fn states(guest: &mut Guest, share: u32, at: Instant) -> Vec<crate::view::ShareState> {
    guest.read(at);
    messages(&mut guest.reliable)
        .into_iter()
        .filter_map(|message| match message {
            Message::ShareState { share: of, state } if of == share => Some(state),
            _ => None,
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Word {
    State(crate::view::ShareState),
    Sound(bool),
    Answer(crate::view::ShareState, bool),
}

// What the host told `guest` of `share` since the last look, in order:
// the sharer's words and the answers to its Watches. Like states and
// sound_to it takes what came, so each look uses one of the three.
fn words(guest: &mut Guest, share: u32, at: Instant) -> Vec<Word> {
    guest.read(at);
    messages(&mut guest.reliable)
        .into_iter()
        .filter_map(|message| match message {
            Message::ShareState { share: of, state } if of == share => Some(Word::State(state)),
            Message::ShareSound { share: of, on } if of == share => Some(Word::Sound(on)),
            Message::WatchAnswer {
                share: of,
                state,
                sound,
            } if of == share => Some(Word::Answer(state, sound)),
            _ => None,
        })
        .collect()
}

// A share's state goes from its sharer to each watcher whose Hello says
// SHARE_STATE_SINCE or later, never to an older one, and to a new
// watcher in the answer to its Watch; a room standing for an older copy
// passes none on and counts each bad, as such a copy does, and one from
// someone who does not share is bad.
#[test]
fn share_state_goes_only_to_watchers_that_read_it() {
    use crate::view::ShareState::{Live, Paused};
    let older = Version {
        major: 1,
        minor: 2,
        patch: 0,
    };
    for host in [older, peer::SHARE_STATE_SINCE] {
        let passes = host >= peer::SHARE_STATE_SINCE;
        let start = Instant::now();
        let mut rig = Rig::new(start);
        rig.host.version = host;
        let mut ana = Guest::new();
        ana.join_as(&mut rig, "Ana", peer::SHARE_STATE_SINCE, None, start);
        let mut bo = Guest::new();
        bo.join_as(&mut rig, "Bo", older, None, start);
        let mut cy = Guest::new();
        cy.join_as(&mut rig, "Cy", peer::SHARE_STATE_SINCE, None, start);
        let share = granted(&mut rig, &mut ana, start);
        let watch = Message::Watch {
            share,
            on: true,
            hevc: true,
        };
        bo.say(&mut rig, &watch, start);
        cy.say(&mut rig, &watch, start);
        let paused = Message::ShareState {
            share,
            state: Paused,
        };
        ana.say(&mut rig, &paused, start);
        assert!(words(&mut bo, share, start).is_empty(), "{host}");
        let told = states(&mut cy, share, start);
        assert_eq!(told, if passes { vec![Paused] } else { vec![] }, "{host}");
        // Said again, it is no news.
        ana.say(&mut rig, &paused, start);
        assert!(states(&mut cy, share, start).is_empty(), "{host}");
        let mut dan = Guest::new();
        dan.join_as(&mut rig, "Dan", peer::SHARE_STATE_SINCE, None, start);
        dan.say(&mut rig, &watch, start);
        let told = words(&mut dan, share, start);
        let answered = if passes {
            vec![Word::Answer(Paused, false)]
        } else {
            vec![]
        };
        assert_eq!(told, answered, "{host}");
        let later = start + sharing::WORDS_EVERY;
        ana.say(&mut rig, &Message::ShareState { share, state: Live }, later);
        for guest in [&mut cy, &mut dan] {
            let told = states(guest, share, later);
            assert_eq!(told, if passes { vec![Live] } else { vec![] }, "{host}");
        }
        // Ana said three.
        let bad = if passes { 0 } else { 3 };
        assert_eq!(rig.host.drops.bad, bad, "{host}");
        cy.say(&mut rig, &paused, later);
        assert_eq!(rig.host.drops.bad, bad + 1, "{host}");
        assert!(states(&mut dan, share, later).is_empty(), "{host}");
    }
}

// A 10 ms stereo frame as a sharer's encoder makes it.
fn stereo_frame() -> Vec<u8> {
    use voice::stereo::{Encoder, FRAME, MAX_PACKET};
    let mut encoder = Encoder::new().unwrap();
    let pcm: Vec<f32> = (0..FRAME * 2)
        .map(|n| 0.2 * ((n / 2) as f32 * 0.06).sin())
        .collect();
    let mut out = [0u8; MAX_PACKET];
    let len = encoder.encode(&pcm, &mut out).unwrap();
    out[..len].to_vec()
}

fn sound_frame(share: u32, seq: u16, frame: &[u8]) -> crate::sound::SoundFrame<'_> {
    crate::sound::SoundFrame {
        slot: 0,
        share,
        seq,
        captured: 1_000_000 + u64::from(seq) * 10_000,
        first: seq == 0,
        last: false,
        frame,
    }
}

// PC sound for `share` from `guest`, sealed for the host.
fn sent_sound(guest: &mut Guest, share: u32, seq: u16, frame: &[u8]) -> Vec<u8> {
    let mut payload = Vec::new();
    sound_frame(share, seq, frame).write_sent(&mut payload);
    seal(
        guest.session.as_mut().expect("joined"),
        Channel::Sound,
        &payload,
    )
}

// What came to `guest` since the last look: each PC sound packet the
// host passed on, whole, and each ShareSound it was told.
fn sound_to(guest: &mut Guest, now: Instant) -> (Vec<Vec<u8>>, Vec<(u32, bool)>) {
    let mut sounds = Vec::new();
    for packet in guest.wire.packets() {
        let mut plain = Vec::new();
        let session = guest.session.as_mut().expect("joined");
        if session.decrypt(&packet, &mut plain).is_err() {
            continue;
        }
        match peer::read_plain(&plain) {
            Some(Plain::Sound(payload)) => sounds.push(payload.to_vec()),
            Some(Plain::Control(frame)) => {
                let _ = guest.reliable.receive(frame, now);
            }
            _ => {}
        }
    }
    let told = messages(&mut guest.reliable)
        .into_iter()
        .filter_map(|message| match message {
            Message::ShareSound { share, on } => Some((share, on)),
            _ => None,
        })
        .collect();
    (sounds, told)
}

// PC sound, and ShareSound, go from the sharer to each watcher whose
// Hello says PC_SOUND_SINCE or later: never to an older one, to someone
// who does not watch, or back to the sharer. A new watcher hears at once
// whether the sound is on, and the sound goes on whatever ShareSound
// says. A room standing for an older copy passes none of it on and
// counts each sound packet and ShareSound bad, as such a copy does, and
// the room's own view says so.
#[test]
fn pc_sound_goes_only_to_watchers_that_read_it() {
    use crate::view::ShareState::Live;
    let older = Version {
        major: 1,
        minor: 2,
        patch: 0,
    };
    let frame = stereo_frame();
    for host in [older, peer::PC_SOUND_SINCE] {
        let passes = host >= peer::PC_SOUND_SINCE;
        let start = Instant::now();
        let mut rig = Rig::new(start);
        rig.host.version = host;
        assert_eq!(
            rig.host.view(start, 0).share.sound_unheard,
            !passes,
            "{host}"
        );
        let mut ana = Guest::new();
        ana.join_as(&mut rig, "Ana", peer::PC_SOUND_SINCE, None, start);
        let mut bo = Guest::new();
        bo.join_as(&mut rig, "Bo", older, None, start);
        let mut cy = Guest::new();
        cy.join_as(&mut rig, "Cy", peer::PC_SOUND_SINCE, None, start);
        let mut dee = Guest::new();
        dee.join_as(&mut rig, "Dee", peer::PC_SOUND_SINCE, None, start);
        let share = granted(&mut rig, &mut ana, start);
        let watch = Message::Watch {
            share,
            on: true,
            hevc: true,
        };
        bo.say(&mut rig, &watch, start);
        cy.say(&mut rig, &watch, start);
        let answered = if passes {
            vec![Word::Answer(Live, false)]
        } else {
            vec![]
        };
        assert_eq!(words(&mut cy, share, start), answered, "{host}");
        for guest in [&mut bo, &mut dee] {
            assert_eq!(sound_to(guest, start), (vec![], vec![]), "{host}");
        }
        let on = Message::ShareSound { share, on: true };
        ana.say(&mut rig, &on, start);
        let from = ana.wire.addr();
        rig.deliver(&sent_sound(&mut ana, share, 0, &frame), from, start);
        let at = peer_at(&mut rig, &ana);
        let slot = rig.host.peers[at].slot;
        let mut relayed = Vec::new();
        sound_frame(share, 0, &frame).write_relayed(slot, &mut relayed);
        let heard = if passes {
            (vec![relayed], vec![(share, true)])
        } else {
            (vec![], vec![])
        };
        assert_eq!(sound_to(&mut cy, start), heard, "{host}");
        for guest in [&mut ana, &mut bo, &mut dee] {
            assert_eq!(sound_to(guest, start), (vec![], vec![]), "{host}");
        }
        // Said again, it is no news.
        ana.say(&mut rig, &on, start);
        assert!(sound_to(&mut cy, start).1.is_empty(), "{host}");
        let mut eve = Guest::new();
        eve.join_as(&mut rig, "Eve", peer::PC_SOUND_SINCE, None, start);
        eve.say(&mut rig, &watch, start);
        let answered = if passes {
            vec![Word::Answer(Live, true)]
        } else {
            vec![]
        };
        assert_eq!(words(&mut eve, share, start), answered, "{host}");
        // Off changes the words, not what is passed on.
        let later = start + sharing::WORDS_EVERY;
        ana.say(&mut rig, &Message::ShareSound { share, on: false }, later);
        rig.deliver(&sent_sound(&mut ana, share, 1, &frame), from, later);
        for guest in [&mut cy, &mut eve] {
            let (sounds, told) = sound_to(guest, later);
            let heard = if passes {
                (1, vec![(share, false)])
            } else {
                (0, vec![])
            };
            assert_eq!((sounds.len(), told), heard, "{host}");
        }
        assert!(sound_to(&mut ana, later).0.is_empty(), "{host}");
        // Two packets and three ShareSounds.
        assert_eq!(rig.host.drops.bad, if passes { 0 } else { 5 }, "{host}");
    }
}

// A sharer can say its sound is on and off faster than a far watcher's
// stream drains: the first change in a while goes on at once, the rest
// at most every WORDS_EVERY, as they stand when the gap ends, and a
// change taken back meanwhile goes nowhere. How the share stands keeps
// to the same gap.
#[test]
fn a_sharers_words_go_on_at_most_every_gap() {
    let start = Instant::now();
    let gap = sharing::WORDS_EVERY;
    let mut rig = Rig::new(start);
    rig.host.version = peer::PC_SOUND_SINCE;
    let mut ana = Guest::new();
    ana.join_as(&mut rig, "Ana", peer::PC_SOUND_SINCE, None, start);
    let mut cy = Guest::new();
    cy.join_as(&mut rig, "Cy", peer::PC_SOUND_SINCE, None, start);
    let share = granted(&mut rig, &mut ana, start);
    let watch = Message::Watch {
        share,
        on: true,
        hevc: true,
    };
    cy.say(&mut rig, &watch, start);
    sound_to(&mut cy, start);
    let said = |on| Message::ShareSound { share, on };
    ana.say(&mut rig, &said(true), start);
    assert_eq!(sound_to(&mut cy, start).1, [(share, true)]);
    for (tenths, on) in [(1, false), (2, true), (3, false)] {
        let at = start + gap * tenths / 10;
        ana.say(&mut rig, &said(on), at);
        assert!(sound_to(&mut cy, at).1.is_empty(), "{tenths}");
    }
    assert_eq!(rig.host.share_deadline(), Some(start + gap));
    rig.tick(start + gap);
    assert_eq!(sound_to(&mut cy, start + gap).1, [(share, false)]);

    let at = start + gap + gap / 10;
    ana.say(&mut rig, &said(true), at);
    ana.say(&mut rig, &said(false), at);
    rig.tick(start + gap * 2);
    assert!(sound_to(&mut cy, start + gap * 2).1.is_empty());

    use crate::view::ShareState::{Live, Paused};
    let stands = |state| Message::ShareState { share, state };
    let at = start + gap * 2;
    ana.say(&mut rig, &stands(Paused), at);
    assert_eq!(states(&mut cy, share, at), [Paused]);
    ana.say(&mut rig, &stands(Live), at + gap / 10);
    assert!(
        states(&mut cy, share, at + gap / 10).is_empty(),
        "within the gap"
    );
    rig.tick(at + gap);
    assert_eq!(states(&mut cy, share, at + gap), [Live]);

    // A watcher who starts while a word waits is told what the others
    // were, so one taken back meanwhile leaves nobody with it.
    let mut dan = Guest::new();
    let at = start + gap * 5;
    dan.join_as(&mut rig, "Dan", peer::PC_SOUND_SINCE, None, at);
    ana.say(&mut rig, &stands(Paused), at);
    ana.say(&mut rig, &stands(Live), at + gap / 10);
    dan.say(&mut rig, &watch, at + gap / 5);
    ana.say(&mut rig, &stands(Paused), at + gap * 3 / 10);
    rig.tick(at + gap);
    assert_eq!(
        words(&mut dan, share, at + gap),
        [Word::Answer(Paused, false)]
    );

    // And so with the sound: a watcher who starts while an on waits is
    // told off, as the others were. An on taken back meanwhile never
    // reaches it, and one that stands does with the others when the gap
    // ends.
    let mut eve = Guest::new();
    let at = start + gap * 7;
    eve.join_as(&mut rig, "Eve", peer::PC_SOUND_SINCE, None, at);
    ana.say(&mut rig, &stands(Live), at);
    ana.say(&mut rig, &said(true), at + gap / 10);
    eve.say(&mut rig, &watch, at + gap / 5);
    ana.say(&mut rig, &said(false), at + gap * 3 / 10);
    rig.tick(at + gap);
    assert_eq!(
        words(&mut eve, share, at + gap),
        [Word::Answer(Live, false)]
    );
    let mut fay = Guest::new();
    let at = start + gap * 9;
    fay.join_as(&mut rig, "Fay", peer::PC_SOUND_SINCE, None, at);
    ana.say(&mut rig, &stands(Paused), at);
    ana.say(&mut rig, &said(true), at + gap / 10);
    fay.say(&mut rig, &watch, at + gap / 5);
    rig.tick(at + gap);
    let told = words(&mut fay, share, at + gap);
    assert_eq!(told, [Word::Answer(Paused, false), Word::Sound(true)]);
    assert_eq!(sound_to(&mut eve, at + gap).1, [(share, true)]);
    assert_eq!(rig.host.drops.bad, 0);
}

// Each Watch that starts a watch is answered once, with both words as
// the other watchers were last told, Live and off included; what the
// sharer says after goes as a change. A Stop, a Watch said again or
// saying what the viewer decodes, and one for a share that ended get no
// answer. A press past the limit gets one, since the watcher waits for
// one for each. An answer from a client is bad, and this host's own
// viewer takes its answer at its press.
#[test]
fn each_watch_that_starts_a_watch_is_answered_once() {
    use crate::view::ShareState::{Live, Paused};
    // The answer carries the state, and goes to whoever reads PC sound.
    assert_eq!(peer::SHARE_STATE_SINCE, peer::PC_SOUND_SINCE);
    let start = Instant::now();
    let gap = sharing::WORDS_EVERY;
    let mut rig = Rig::new(start);
    rig.host.version = peer::PC_SOUND_SINCE;
    let mut ana = Guest::new();
    ana.join_as(&mut rig, "Ana", peer::PC_SOUND_SINCE, None, start);
    let mut cy = Guest::new();
    cy.join_as(&mut rig, "Cy", peer::PC_SOUND_SINCE, None, start);
    let share = granted(&mut rig, &mut ana, start);
    let watch = |on, hevc| Message::Watch { share, on, hevc };
    let stands = |state| Message::ShareState { share, state };
    cy.say(&mut rig, &watch(true, true), start);
    assert_eq!(words(&mut cy, share, start), [Word::Answer(Live, false)]);

    ana.say(&mut rig, &stands(Paused), start);
    ana.say(&mut rig, &Message::ShareSound { share, on: true }, start);
    let at = start + gap;
    rig.tick(at);
    assert_eq!(
        words(&mut cy, share, at),
        [Word::State(Paused), Word::Sound(true)]
    );
    cy.say(&mut rig, &watch(false, true), at);
    cy.say(&mut rig, &watch(true, true), at);
    assert_eq!(words(&mut cy, share, at), [Word::Answer(Paused, true)]);
    // What its viewer decodes, then the same Watch again.
    cy.say(&mut rig, &watch(true, false), at);
    cy.say(&mut rig, &watch(true, false), at);
    assert!(words(&mut cy, share, at).is_empty());
    // Well past WATCH_BURST, at one instant.
    for _ in 0..8 {
        cy.say(&mut rig, &watch(false, true), at);
        cy.say(&mut rig, &watch(true, true), at);
    }
    assert_eq!(words(&mut cy, share, at), [Word::Answer(Paused, true); 8]);
    let answer = Message::WatchAnswer {
        share,
        state: Live,
        sound: false,
    };
    cy.say(&mut rig, &answer, at);
    assert_eq!(rig.host.drops.bad, 1);

    // How the share stood when this host watched, and Live once it is
    // live again.
    assert!(rig.host.watch(share, true, at, &rig.socket));
    let view = rig.host.view(at, 0).share;
    assert_eq!(
        (view.watched_state, view.watched_sound),
        (Paused, Some(true))
    );
    assert!(rig.host.watch(share, false, at, &rig.socket));
    let later = at + gap;
    ana.say(&mut rig, &stands(Live), later);
    assert_eq!(words(&mut cy, share, later), [Word::State(Live)]);
    assert!(rig.host.watch(share, true, later, &rig.socket));
    assert_eq!(rig.host.view(later, 0).share.watched_state, Live);

    // A share that ended while the Watch was on its way.
    ana.say(&mut rig, &Message::ShareStop, later);
    cy.say(&mut rig, &watch(true, true), later);
    assert!(words(&mut cy, share, later).is_empty());
    assert_eq!(rig.host.drops.bad, 1);
}

// Only the sharer of the live share sends its PC sound and says whether
// it goes out. Sound from anyone else, for share 0 or for a share that
// is not the sender's, is bad, and so is ShareSound from anyone else.
// What the sharer sent just before its share ended goes nowhere and is
// not bad, as video is not; a second later it is.
#[test]
fn pc_sound_only_from_its_sharer() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    rig.host.version = peer::PC_SOUND_SINCE;
    let mut ana = Guest::new();
    ana.join_as(&mut rig, "Ana", peer::PC_SOUND_SINCE, None, start);
    let mut bo = Guest::new();
    bo.join_as(&mut rig, "Bo", peer::PC_SOUND_SINCE, None, start);
    let mut cy = Guest::new();
    cy.join_as(&mut rig, "Cy", peer::PC_SOUND_SINCE, None, start);
    let frame = stereo_frame();
    let watch = |share| Message::Watch {
        share,
        on: true,
        hevc: true,
    };
    let share = granted(&mut rig, &mut ana, start);
    cy.say(&mut rig, &watch(share), start);
    sound_to(&mut cy, start);
    let (from_ana, from_bo) = (ana.wire.addr(), bo.wire.addr());
    rig.deliver(&sent_sound(&mut bo, share, 0, &frame), from_bo, start);
    assert_eq!(rig.host.drops.bad, 1);
    rig.deliver(&sent_sound(&mut ana, 0, 0, &frame), from_ana, start);
    assert_eq!(rig.host.drops.bad, 2);
    rig.deliver(
        &sent_sound(&mut ana, share.wrapping_add(1).max(1), 0, &frame),
        from_ana,
        start,
    );
    assert_eq!(rig.host.drops.bad, 3);
    bo.say(&mut rig, &Message::ShareSound { share, on: true }, start);
    assert_eq!(rig.host.drops.bad, 4);
    assert_eq!(sound_to(&mut cy, start), (vec![], vec![]));

    ana.say(&mut rig, &Message::ShareStop, start);
    let late = start + Duration::from_millis(100);
    rig.deliver(&sent_sound(&mut ana, share, 1, &frame), from_ana, late);
    assert_eq!(rig.host.drops.bad, 4);
    let later = start + Duration::from_secs(2);
    rig.deliver(&sent_sound(&mut ana, share, 2, &frame), from_ana, later);
    assert_eq!(rig.host.drops.bad, 5);
    assert!(sound_to(&mut cy, later).0.is_empty());

    // ShareSound about a share of theirs that ended was on its way.
    let next = granted(&mut rig, &mut ana, later);
    cy.say(&mut rig, &watch(next), later);
    sound_to(&mut cy, later);
    ana.say(&mut rig, &Message::ShareSound { share, on: true }, later);
    assert_eq!(rig.host.drops.bad, 5);
    assert!(sound_to(&mut cy, later).1.is_empty());
    let on = Message::ShareSound {
        share: next,
        on: true,
    };
    ana.say(&mut rig, &on, later);
    assert_eq!(sound_to(&mut cy, later).1, [(next, true)]);
    assert_eq!(rig.host.drops.bad, 5);
}

// A friend's PC can send as fast as its link goes. With a clock that
// stands still, a sharer gets its burst of 50 sound packets, then 150 a
// second; the rest is dropped and counted, and nothing of it counts as
// bad.
#[test]
fn sharer_sound_burst_and_rate() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    rig.host.version = peer::PC_SOUND_SINCE;
    let mut ana = Guest::new();
    ana.join_as(&mut rig, "Ana", peer::PC_SOUND_SINCE, None, start);
    let mut bo = Guest::new();
    bo.join_as(&mut rig, "Bo", peer::PC_SOUND_SINCE, None, start);
    let share = granted(&mut rig, &mut ana, start);
    bo.say(
        &mut rig,
        &Message::Watch {
            share,
            on: true,
            hevc: true,
        },
        start,
    );
    sound_to(&mut bo, start);
    let frame = stereo_frame();
    let from = ana.wire.addr();
    let dropped = rig.host.screen.dropped;
    for seq in 0..60 {
        let packet = sent_sound(&mut ana, share, seq, &frame);
        rig.deliver(&packet, from, start);
    }
    assert_eq!(sound_to(&mut bo, start).0.len(), 50);
    assert_eq!(rig.host.screen.dropped - dropped, 10);
    let later = start + Duration::from_millis(100);
    for seq in 60..90 {
        let packet = sent_sound(&mut ana, share, seq, &frame);
        rig.deliver(&packet, from, later);
    }
    assert_eq!(sound_to(&mut bo, later).0.len(), 15);
    assert_eq!(rig.host.screen.dropped - dropped, 10 + 15);
    assert_eq!(rig.host.drops.bad, 0);
}

// This host, watching a friend's share, hears its PC sound from where it
// passes it on, and only then. Told at its watch that the sound is off,
// as the friend has not said on, it hears the first frame that comes all
// the same: the first word of a watch only sets the word. The friend's
// word that it is off lets the ear go, a frame that starts a spell
// brings it back, and the end of the watch lets it go for good.
#[test]
fn the_host_hears_the_share_it_watches() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    rig.host.version = peer::PC_SOUND_SINCE;
    let (speaker, orders) = crossbeam_channel::unbounded();
    rig.host.sound = Sound::new(speaker, rig.host.clock, Log::off(), start);
    let mut ana = Guest::new();
    ana.join_as(&mut rig, "Ana", peer::PC_SOUND_SINCE, None, start);
    let ana_key = *ana.identity.public();
    let share = granted(&mut rig, &mut ana, start);
    let frame = stereo_frame();
    let from = ana.wire.addr();
    rig.deliver(&sent_sound(&mut ana, share, 0, &frame), from, start);
    assert!(orders.try_recv().is_err(), "heard before watching");

    assert!(rig.host.watch(share, true, start, &rig.socket));
    assert_eq!(rig.host.view(start, 0).share.watched_sound, Some(false));
    rig.deliver(&sent_sound(&mut ana, share, 1, &frame), from, start);
    match orders.try_recv() {
        Ok(ToSpeaker::AddStream(key, ear, gain)) => {
            assert_eq!((key, gain), (ana_key, 1.0));
            assert_eq!(ear.lock().unwrap().stats().held_frames, 1);
        }
        _ => panic!("nothing for the speakers"),
    }
    assert!(rig.host.view(start, 0).numbers.sound.is_some());
    ana.say(&mut rig, &Message::ShareSound { share, on: true }, start);
    assert_eq!(rig.host.view(start, 0).share.watched_sound, Some(true));

    let later = start + sharing::WORDS_EVERY;
    ana.say(&mut rig, &Message::ShareSound { share, on: false }, later);
    assert!(matches!(
        orders.try_recv(),
        Ok(ToSpeaker::RemoveStream(key)) if key == ana_key
    ));
    let view = rig.host.view(later, 0);
    assert_eq!(view.share.watched_sound, Some(false));
    assert!(view.numbers.sound.is_none());
    rig.deliver(&sent_sound(&mut ana, share, 2, &frame), from, later);
    assert!(orders.try_recv().is_err(), "a frame from before off");
    // Frame 0 is the one that starts a spell.
    rig.deliver(&sent_sound(&mut ana, share, 0, &frame), from, later);
    assert!(matches!(
        orders.try_recv(),
        Ok(ToSpeaker::AddStream(key, _, _)) if key == ana_key
    ));

    assert!(rig.host.watch(share, false, later, &rig.socket));
    assert!(matches!(
        orders.try_recv(),
        Ok(ToSpeaker::RemoveStream(key)) if key == ana_key
    ));
    rig.deliver(&sent_sound(&mut ana, share, 3, &frame), from, later);
    assert!(orders.try_recv().is_err(), "heard after watching");
    let view = rig.host.view(later, 0);
    assert!(view.numbers.sound.is_none());
    assert_eq!(view.numbers.sound_dropped, 1);
    assert_eq!(rig.host.drops.bad, 0);
}

// A friend turns PC sound on just after their share goes on again, and
// the word waits out the gap. This host, starting to watch meanwhile, is
// told off and still hears at once a frame from the middle of a spell:
// the first word of a watch only sets the word. The on that goes when
// the gap ends keeps the ear.
#[test]
fn the_host_hears_at_once_while_the_on_waits() {
    use crate::view::ShareState::{Live, Paused};
    let start = Instant::now();
    let gap = sharing::WORDS_EVERY;
    let mut rig = Rig::new(start);
    rig.host.version = peer::PC_SOUND_SINCE;
    let (speaker, orders) = crossbeam_channel::unbounded();
    rig.host.sound = Sound::new(speaker, rig.host.clock, Log::off(), start);
    let mut ana = Guest::new();
    ana.join_as(&mut rig, "Ana", peer::PC_SOUND_SINCE, None, start);
    let ana_key = *ana.identity.public();
    let share = granted(&mut rig, &mut ana, start);
    let frame = stereo_frame();
    let from = ana.wire.addr();
    let stands = |state| Message::ShareState { share, state };
    ana.say(&mut rig, &stands(Paused), start);
    let resumed = start + gap;
    ana.say(&mut rig, &stands(Live), resumed);
    let on = Message::ShareSound { share, on: true };
    ana.say(&mut rig, &on, resumed + gap / 10);
    assert_eq!(rig.host.share_deadline(), Some(resumed + gap));

    let at = resumed + gap / 5;
    assert!(rig.host.watch(share, true, at, &rig.socket));
    assert_eq!(rig.host.view(at, 0).share.watched_sound, Some(false));
    rig.deliver(&sent_sound(&mut ana, share, 7, &frame), from, at);
    match orders.try_recv() {
        Ok(ToSpeaker::AddStream(key, ear, _)) => {
            assert_eq!(key, ana_key);
            assert_eq!(ear.lock().unwrap().stats().held_frames, 1);
        }
        _ => panic!("nothing for the speakers"),
    }
    assert_eq!(rig.host.view(at, 0).share.watched_sound, Some(false));

    rig.tick(resumed + gap);
    assert!(orders.try_recv().is_err(), "the on let the ear go");
    let view = rig.host.view(resumed + gap, 0);
    assert_eq!(view.share.watched_sound, Some(true));
    assert!(view.numbers.sound.is_some());
    assert_eq!(view.numbers.sound_dropped, 0);
    assert_eq!(rig.host.drops.bad, 0);
}

// A friend turns PC sound off within the gap, and the word waits for its
// end. The timer pass at the gap's end tells this host's own viewer and
// says the view changed. The timer thread stores a view only when a pass
// says so, and if this one did not, the panel would show the word up to
// a ping late. The pass just before has nothing to show.
#[test]
fn a_held_back_word_redraws_the_hosts_panel() {
    let start = Instant::now();
    let gap = sharing::WORDS_EVERY;
    let mut rig = Rig::new(start);
    rig.host.version = peer::PC_SOUND_SINCE;
    let mut ana = Guest::new();
    ana.join_as(&mut rig, "Ana", peer::PC_SOUND_SINCE, None, start);
    let share = granted(&mut rig, &mut ana, start);
    assert!(rig.host.watch(share, true, start, &rig.socket));
    let said = |on| Message::ShareSound { share, on };
    ana.say(&mut rig, &said(true), start);
    assert_eq!(rig.host.view(start, 0).share.watched_sound, Some(true));
    let at = start + gap / 10;
    ana.say(&mut rig, &said(false), at);
    // Whatever else is due, a ping among it, goes on this pass.
    rig.tick(at);

    let almost = start + gap - Duration::from_millis(1);
    assert!(!rig.host.on_timer(almost, &rig.socket));
    assert_eq!(rig.host.view(almost, 0).share.watched_sound, Some(true));
    let end = start + gap;
    assert!(rig.host.on_timer(end, &rig.socket));
    assert_eq!(rig.host.view(end, 0).share.watched_sound, Some(false));
}

// The volume this host plays a friend's PC sound at goes with every ear
// made for it. Set before any sound came, it goes with the first ear. A
// change reaches the speakers at once. An ear that a FIRST frame makes
// after the friend said off gets it too, and once the volume is back at
// 100 the next ear goes at 1.0.
#[test]
fn a_pc_sound_volume_goes_with_every_ear() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    rig.host.version = peer::PC_SOUND_SINCE;
    let (speaker, orders) = crossbeam_channel::unbounded();
    rig.host.sound = Sound::new(speaker, rig.host.clock, Log::off(), start);
    let mut ana = Guest::new();
    ana.join_as(&mut rig, "Ana", peer::PC_SOUND_SINCE, None, start);
    let ana_key = *ana.identity.public();
    let share = granted(&mut rig, &mut ana, start);
    let frame = stereo_frame();
    let from = ana.wire.addr();
    let told = || -> Vec<(&str, f32)> {
        orders
            .try_iter()
            .map(|order| match order {
                ToSpeaker::AddStream(key, _, gain) if key == ana_key => ("add", gain),
                ToSpeaker::StreamVolume(key, gain) if key == ana_key => ("volume", gain),
                ToSpeaker::RemoveStream(key) if key == ana_key => ("remove", 0.0),
                _ => ("other", 0.0),
            })
            .collect()
    };

    rig.host.set_sound_volume(ana_key, 40);
    // Told off at each watch, the host's viewer makes an ear from the
    // first frame that comes: the first word of a watch only sets the
    // word.
    assert!(rig.host.watch(share, true, start, &rig.socket));
    assert_eq!(rig.host.view(start, 0).share.watched_sound, Some(false));
    rig.deliver(&sent_sound(&mut ana, share, 1, &frame), from, start);
    assert_eq!(told(), [("volume", 0.4), ("add", 0.4)]);
    ana.say(&mut rig, &Message::ShareSound { share, on: true }, start);
    rig.host.set_sound_volume(ana_key, 150);
    assert_eq!(told(), [("volume", 1.5)]);

    // Frame 0 is the one that starts a spell.
    let later = start + sharing::WORDS_EVERY;
    ana.say(&mut rig, &Message::ShareSound { share, on: false }, later);
    rig.deliver(&sent_sound(&mut ana, share, 0, &frame), from, later);
    assert_eq!(told(), [("remove", 0.0), ("add", 1.5)]);

    rig.host.set_sound_volume(ana_key, 100);
    assert!(rig.host.watch(share, false, later, &rig.socket));
    assert!(rig.host.watch(share, true, later, &rig.socket));
    assert_eq!(rig.host.view(later, 0).share.watched_sound, Some(false));
    rig.deliver(&sent_sound(&mut ana, share, 2, &frame), from, later);
    assert_eq!(told(), [("volume", 1.0), ("remove", 0.0), ("add", 1.0)]);
    assert_eq!(rig.host.drops.bad, 0);
}

// A watcher picking caps as fast as it can: the sharer is told at most
// LIMITS_PER_GAP changes in a SWITCH_GAP, a new watcher's first two at
// once, and the last pick still reaches it once the gap allows, past any
// limit on the watcher's messages.
#[test]
fn caps_picked_fast_reach_the_sharer_paced() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    rig.host.version = peer::LIMITS_SINCE;
    let mut ana = Guest::new();
    ana.join_as(&mut rig, "Ana", peer::LIMITS_SINCE, None, start);
    let mut bo = Guest::new();
    bo.join_as(&mut rig, "Bo", peer::LIMITS_SINCE, None, start);
    let share = granted(&mut rig, &mut ana, start);
    let told = |messages: Vec<Message>| -> Vec<control::ShareLimits> {
        messages
            .into_iter()
            .filter_map(|message| match message {
                Message::ShareLimits {
                    limits: Some(limits),
                    ..
                } => Some(limits),
                _ => None,
            })
            .collect()
    };
    ana.heard(start);
    bo.say(
        &mut rig,
        &Message::Watch {
            share,
            on: true,
            hevc: true,
        },
        start,
    );
    let mut at = start;
    let mut last = None;
    for n in 0..20u16 {
        at += Duration::from_millis(50);
        let height = if n % 2 == 0 { 1080 } else { 1440 };
        let limits = control::WatchLimits {
            takes_4k: true,
            height: Some(height),
            ..control::WatchLimits::default()
        };
        bo.say(&mut rig, &Message::WatchLimits { share, limits }, at);
        last = Some(height);
    }
    // The share's start told the first fold already, so of Bo's picks
    // only the first goes in this gap.
    let first = told(ana.heard(at));
    assert_eq!(
        first.iter().map(|limits| limits.height).collect::<Vec<_>>(),
        [1080]
    );
    rig.tick(start + share::SWITCH_GAP - Duration::from_millis(1));
    assert!(told(ana.heard(at)).is_empty());
    let after = start + share::SWITCH_GAP;
    rig.tick(after);
    let later = told(ana.heard(after));
    assert_eq!(
        later.iter().map(|limits| limits.height).collect::<Vec<_>>(),
        [last.unwrap()]
    );
    assert_eq!(rig.host.drops.bad, 0);
}

// A watcher whose copy reads limits hears once that the sharer's does
// not, so its menu can say why a cap does nothing.
#[test]
fn the_watcher_of_an_older_sharer_hears_its_cap_goes_unheard() {
    let older = Version {
        major: 1,
        minor: 1,
        patch: 0,
    };
    let start = Instant::now();
    let mut rig = Rig::new(start);
    rig.host.version = peer::LIMITS_SINCE;
    let mut ana = Guest::new();
    ana.join_as(&mut rig, "Ana", older, None, start);
    let mut bo = Guest::new();
    bo.join_as(&mut rig, "Bo", peer::LIMITS_SINCE, None, start);
    let share = granted(&mut rig, &mut ana, start);
    bo.say(
        &mut rig,
        &Message::Watch {
            share,
            on: true,
            hevc: true,
        },
        start,
    );
    let limits = control::WatchLimits::default();
    bo.say(&mut rig, &Message::WatchLimits { share, limits }, start);
    bo.read(start);
    let told: Vec<_> = messages(&mut bo.reliable)
        .into_iter()
        .filter_map(|message| match message {
            Message::ShareLimits { share, limits } => Some((share, limits)),
            _ => None,
        })
        .collect();
    assert_eq!(told, [(share, None)]);
}

// A sharer whose Hello is older than FEEDBACK_SINCE reads recover
// requests on the control stream only, a newer one on the feedback
// stream. A watcher's feedback is taken from either, and nothing else
// from the feedback stream.
#[test]
fn feedback_goes_where_the_sharers_version_reads_it() {
    let older = Version {
        major: 0,
        minor: 2,
        patch: 1,
    };
    for (version, on_control) in [(older, true), (peer::FEEDBACK_SINCE, false)] {
        let start = Instant::now();
        let mut rig = Rig::new(start);
        let mut ana = Guest::new();
        ana.join_as(&mut rig, "Ana", version, None, start);
        let mut bo = Guest::new();
        bo.join(&mut rig, "Bo", start);
        let share = granted(&mut rig, &mut ana, start);
        bo.say(
            &mut rig,
            &Message::Watch {
                share,
                on: true,
                hevc: true,
            },
            start,
        );
        ana.heard(start);
        let from = ana.wire.addr();
        for number in 0..=9 {
            let packet = sent_video(&mut ana, &frame_packet(number));
            rig.deliver(&packet, from, start);
        }
        let recover = |first, last| Message::Recover { share, first, last };
        bo.say(&mut rig, &recover(2, 3), start);
        bo.say_feedback(&mut rig, &recover(6, 7), start);
        bo.say_feedback(&mut rig, &Message::ShareStop, start);
        assert_eq!(rig.host.drops.bad, 1, "{version}");
        assert!(rig.host.live.is_some(), "{version}: still sharing");

        ana.read(start);
        let (control, feedback) = (messages(&mut ana.reliable), messages(&mut ana.feedback));
        let (read, unread) = if on_control {
            (control, feedback)
        } else {
            (feedback, control)
        };
        assert_eq!(recovers_in(&read), [(2, 3), (6, 7)], "{version}");
        assert_eq!(recovers_in(&unread), [], "{version}");
    }
}

// On their own, or in the facts that count a new watcher.
fn idr_asks_in(messages: &[Message]) -> usize {
    messages
        .iter()
        .filter(|message| match message {
            Message::Idr { .. } => true,
            Message::ShareFacts(facts) => facts.idr,
            _ => false,
        })
        .count()
}

// Leaving and joining again gets a friend a new Peer, but not new
// limits: a Watch right after rejoining asks the sharer for no IDR
// while the last one is not IDR_ASK_EVERY old.
#[test]
fn rejoin_keeps_share_limits() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let mut bo = Guest::new();
    bo.join(&mut rig, "Bo", start);
    let share = granted(&mut rig, &mut ana, start);
    ana.heard(start);
    let mut asks = Vec::new();
    for step in 0..8u64 {
        let at = start + Duration::from_millis(step * 100);
        if step > 0 {
            bo.say(&mut rig, &Message::Bye, at);
            bo.left();
            bo.join(&mut rig, "Bo", at);
        }
        bo.say(
            &mut rig,
            &Message::Watch {
                share,
                on: true,
                hevc: true,
            },
            at,
        );
        asks.push(idr_asks_in(&ana.heard(at)));
    }
    println!("IDR asks from Bo watching, leaving and joining again every 100 ms: {asks:?}");
    assert_eq!(asks, [1, 0, 0, 0, 0, 1, 0, 0]);
    assert_eq!(rig.host.peers.len(), 2);
}

// Each ask for control puts a request in front of the sharer, so
// leaving and joining again gets a friend no fresh asks either.
#[test]
fn rejoin_keeps_control_limits() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let mut bo = Guest::new();
    bo.join(&mut rig, "Bo", start);
    let share = granted(&mut rig, &mut ana, start);
    ana.heard(start);
    let mut shown = 0;
    let mut too_soon = 0;
    for step in 0..8u32 {
        let at = start + Duration::from_millis(u64::from(step) * 10);
        if step > 0 {
            bo.say(&mut rig, &Message::Bye, at);
            bo.left();
            bo.join(&mut rig, "Bo", at);
        }
        let watch = Message::Watch {
            share,
            on: true,
            hevc: true,
        };
        bo.say(&mut rig, &watch, at);
        bo.say(
            &mut rig,
            &Message::ControlAsk {
                share,
                ask: step + 1,
            },
            at,
        );
        shown += ana
            .heard(at)
            .iter()
            .filter(|message| matches!(message, Message::ControlAsked { .. }))
            .count();
        too_soon += bo
            .heard(at)
            .iter()
            .filter(|message| {
                matches!(
                    message,
                    Message::ControlAnswer {
                        answer: control::ControlAnswer::TooSoon,
                        ..
                    }
                )
            })
            .count();
    }
    println!(
        "Bo asking for control, leaving and joining again every 10 ms: {shown} requests shown, {too_soon} refused as too soon"
    );
    assert_eq!(shown, crate::remote::ASK_BURST as usize);
    assert_eq!(too_soon, 8 - shown);
}

// Nor fresh lines: what a friend may say runs on from where it stood, so
// a Bye between floods gets nothing that waiting would not.
#[test]
fn rejoin_keeps_chat_limit() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let mut bo = Guest::new();
    bo.join(&mut rig, "Bo", start);

    let burst = chat::SAY_BURST as usize;
    let per_second = chat::SAYS_PER_SECOND as usize;
    let later = start + Duration::from_secs(1);
    let mut n = 0;
    for (step, now) in [(0, start), (1, start), (2, later)] {
        if step > 0 {
            ana.say(&mut rig, &Message::Bye, start);
            ana.left();
            ana.join(&mut rig, "Ana", now);
        }
        for _ in 0..burst {
            ana.chat(&mut rig, &say(&format!("line {n}")), now);
            // The host's acks, so the window never holds a line back.
            ana.read(now);
            n += 1;
        }
    }

    let want: Vec<String> = (0..burst)
        .chain(2 * burst..2 * burst + per_second)
        .map(|n| format!("line {n}"))
        .collect();
    assert_eq!(said_only(bo.said_all(&mut rig, later)), want);
}

// The same for the rest of what a friend sends: a bucket left empty is
// still empty on the way back in, and fills as if they had stayed.
#[test]
fn rejoin_keeps_voice_and_handshake_limits() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);

    let spent = |peer: &mut Peer, now| {
        [
            !peer.voice.take(now, VOICE_PER_SECOND, VOICE_BURST),
            !peer
                .voice_reports
                .take(now, VOICE_REPORTS_PER_SECOND, VOICE_REPORT_BURST),
            !peer.handshakes.take(now, REKEYS_PER_SECOND, REKEY_BURST),
        ]
    };
    let peer = &mut rig.host.peers[0];
    while spent(peer, start) != [true; 3] {}

    ana.say(&mut rig, &Message::Bye, start);
    ana.left();
    ana.join(&mut rig, "Ana", start);
    assert_eq!(spent(&mut rig.host.peers[0], start), [true; 3]);

    let later = start + Duration::from_secs(1);
    ana.say(&mut rig, &Message::Bye, start);
    ana.left();
    ana.join(&mut rig, "Ana", later);
    assert_eq!(spent(&mut rig.host.peers[0], later), [false; 3]);
}

// Going round every key one invite lets in, one in the room at a time,
// pushes none of their limits out.
#[test]
fn rejoin_keeps_limits_round_all_of_an_invites_keys() {
    let mut now = Instant::now();
    let mut rig = Rig::new(now);
    let mut guests: Vec<Guest> = (0..crate::invites::KEYS_PER_MULTI_USE)
        .map(|_| Guest::new())
        .collect();
    for guest in &mut guests {
        // No faster than initiations from one address may come.
        now += Duration::from_millis(100);
        guest.join(&mut rig, "Friend", now);
        let says = &mut rig.host.peers[0].says;
        while says.take(now, chat::SAYS_PER_SECOND, chat::SAY_BURST) {}
        guest.say(&mut rig, &Message::Bye, now);
        guest.left();
    }

    now += Duration::from_millis(100);
    guests[0].join(&mut rig, "Friend", now);
    let says = &mut rig.host.peers[0].says;
    let burst = chat::SAY_BURST as usize;
    let lines = (0..burst)
        .filter(|_| says.take(now, chat::SAYS_PER_SECOND, chat::SAY_BURST))
        .count();
    assert!(lines < burst, "{lines}");
}

// A client watching hears of a new frame rate from the roster; this
// host's own viewer has to be told.
#[test]
fn own_viewer_hears_new_fps() {
    use crate::screen::{Batch, WatchEvent};

    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let share = granted(&mut rig, &mut ana, start);
    assert!(rig.host.watch(share, true, start, &rig.socket));
    ana.say(&mut rig, &Message::ShareStart { fps: 60 }, start);
    let mut batch = Batch::default();
    rig.host.screen.watching.take(&mut batch);
    assert_eq!(
        batch.events,
        [
            WatchEvent::Started {
                share,
                fps: 120,
                name: String::from("Ana")
            },
            WatchEvent::Fps { share, fps: 60 }
        ]
    );
}

// The host's own share asked for 120 runs stepped down at 60, which its
// thread says and the roster shows. Picking 60 makes 60 the rate asked,
// so a step back up opens at 60; picking 120 again changes nothing the
// watchers see until the thread runs at it.
#[test]
fn own_share_asked_and_running_fps() {
    use crate::screen::{OwnShare, ShareNews};

    let start = Instant::now();
    let mut rig = Rig::new(start);
    assert!(rig.host.share(120, start, &rig.socket));
    let share = rig
        .host
        .live
        .as_ref()
        .map(|live| live.number)
        .expect("sharing");
    let runs_at = |rig: &mut Rig, fps: u8| {
        rig.host.screen.sharing.tell(ShareNews::Fps { share, fps });
        rig.host.screen_work(start, &rig.socket);
    };
    let rates = |rig: &Rig| {
        let asked = match rig.host.screen.sharing.state() {
            OwnShare::Sharing { fps, .. } => fps,
            other => panic!("{other:?}"),
        };
        (asked, rig.host.live.as_ref().map_or(0, |live| live.fps))
    };
    runs_at(&mut rig, 60);
    assert_eq!(rates(&rig), (120, 60));
    assert!(rig.host.share(60, start, &rig.socket));
    assert_eq!(rates(&rig), (60, 60));
    assert!(rig.host.share(120, start, &rig.socket));
    assert_eq!(rates(&rig), (120, 60), "the roster waits for the thread");
    runs_at(&mut rig, 120);
    assert_eq!(rates(&rig), (120, 120));
}

// Bo presses Watch, Stop watching, and Watch again 300 ms later. The
// last Watch's IDR waits for the half second to end instead of being
// dropped: on a still screen nothing else would bring Bo a picture, and
// without it the sharer sends no pointer shape. An ask about a frame
// inside the gap is dropped as before, and Stop watching takes a held
// one back.
#[test]
fn watch_in_idr_gap_waits_for_it() {
    use crate::screen::IDR_ASK_EVERY;

    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let mut bo = Guest::new();
    bo.join(&mut rig, "Bo", start);
    let share = granted(&mut rig, &mut ana, start);
    let idrs = |messages: Vec<Message>| -> Vec<Option<u32>> {
        messages
            .into_iter()
            .filter_map(|message| match message {
                Message::Idr { seen, .. } => Some(seen),
                Message::ShareFacts(facts) if facts.idr => Some(None),
                _ => None,
            })
            .collect()
    };
    bo.say(
        &mut rig,
        &Message::Watch {
            share,
            on: true,
            hevc: true,
        },
        start,
    );
    assert_eq!(idrs(ana.heard(start)), [None]);
    bo.say(
        &mut rig,
        &Message::Watch {
            share,
            on: false,
            hevc: true,
        },
        start,
    );
    let again = start + Duration::from_millis(300);
    bo.say(
        &mut rig,
        &Message::Watch {
            share,
            on: true,
            hevc: true,
        },
        again,
    );
    let seen = Message::Idr {
        share,
        seen: Some(4),
    };
    bo.say(&mut rig, &seen, again);
    assert!(idrs(ana.heard(again)).is_empty());
    let due = start + IDR_ASK_EVERY;
    assert_eq!(rig.host.share_deadline(), Some(due));
    rig.tick(due - Duration::from_millis(1));
    assert!(idrs(ana.heard(due)).is_empty());
    rig.tick(due);
    assert_eq!(idrs(ana.heard(due)), [None]);
    assert_eq!(rig.host.share_deadline(), None);

    let later = due + Duration::from_millis(100);
    bo.say(
        &mut rig,
        &Message::Watch {
            share,
            on: false,
            hevc: true,
        },
        later,
    );
    bo.say(
        &mut rig,
        &Message::Watch {
            share,
            on: true,
            hevc: true,
        },
        later,
    );
    bo.say(
        &mut rig,
        &Message::Watch {
            share,
            on: false,
            hevc: true,
        },
        later,
    );
    rig.tick(due + IDR_ASK_EVERY);
    assert!(idrs(ana.heard(due + IDR_ASK_EVERY)).is_empty());
    assert_eq!(rig.host.share_deadline(), None);
}

// A friend pressing Watch and Stop watching faster than the limit still
// gets what they pressed last, so the host and they agree about who
// watches. The sharer hears the first change at once and the rest at
// most every FACTS_EVERY, and gets one IDR ask for all of it.
#[test]
fn watch_past_limit_still_counts() {
    use crate::screen::FACTS_EVERY;

    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let mut bo = Guest::new();
    bo.join(&mut rig, "Bo", start);
    let share = granted(&mut rig, &mut ana, start);
    ana.heard(start);
    let at = start + FACTS_EVERY;
    for press in 0..21 {
        let on = press % 2 == 0;
        bo.say(
            &mut rig,
            &Message::Watch {
                share,
                on,
                hevc: true,
            },
            at,
        );
    }
    let b = peer_at(&mut rig, &bo);
    assert_eq!(rig.host.peers[b].share.watching, Some(share));
    let heard = ana.heard(at);
    assert_eq!(idr_asks_in(&heard), 1);
    assert_eq!(
        facts_in(&heard)
            .iter()
            .map(|f| f.watchers)
            .collect::<Vec<_>>(),
        [1]
    );

    let soon = at + Duration::from_millis(100);
    bo.say(
        &mut rig,
        &Message::Watch {
            share,
            on: false,
            hevc: true,
        },
        soon,
    );
    assert_eq!(rig.host.peers[b].share.watching, None);
    assert!(facts_in(&ana.heard(soon)).is_empty());
    let due = at + FACTS_EVERY;
    assert_eq!(rig.host.share_deadline(), Some(due));
    rig.tick(due);
    assert_eq!(
        facts_in(&ana.heard(due))
            .iter()
            .map(|f| f.watchers)
            .collect::<Vec<_>>(),
        [0]
    );
}

// A share goes in HEVC only while every watcher decodes it. A watcher who
// does not makes the facts say so at once, inside FACTS_EVERY too, with
// its IDR ask in them, so the sharer reads both before the same frame and
// the new encoder's first frame is the one IDR the watcher needs. A
// watcher whose facts wait for FACTS_EVERY changes no codec, and its ask
// goes alone. A Watch said again with another answer changes the facts
// and asks for no IDR, and HEVC comes back when the last watcher without
// it stops, in the facts' own time.
#[test]
fn watcher_without_hevc() {
    use crate::screen::FACTS_EVERY;

    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let mut bo = Guest::new();
    bo.join(&mut rig, "Bo", start);
    let mut cy = Guest::new();
    cy.join(&mut rig, "Cy", start);
    let mut dee = Guest::new();
    dee.join(&mut rig, "Dee", start);
    let share = granted(&mut rig, &mut ana, start);
    let watch = |on, hevc| Message::Watch { share, on, hevc };
    // The sharer's view of what it heard, in order.
    let told_in = |messages: Vec<Message>| -> Vec<String> {
        messages
            .into_iter()
            .filter_map(|message| match message {
                Message::ShareFacts(facts) => Some(format!(
                    "{} watching, {}{}",
                    facts.watchers,
                    if facts.hevc { "HEVC" } else { "H.264" },
                    if facts.idr { ", IDR" } else { "" }
                )),
                Message::Idr { seen: None, .. } => Some(String::from("IDR alone")),
                _ => None,
            })
            .collect()
    };

    let at = start + FACTS_EVERY;
    cy.say(&mut rig, &watch(true, true), at);
    assert_eq!(told_in(ana.heard(at)), ["1 watching, HEVC, IDR"]);
    let soon = at + Duration::from_millis(10);
    bo.say(&mut rig, &watch(true, false), soon);
    assert_eq!(told_in(ana.heard(soon)), ["2 watching, H.264, IDR"]);

    // Bo's viewer finds out it does after all: no IDR, and the change
    // waits for FACTS_EVERY like any other.
    let then = soon + Duration::from_millis(10);
    bo.say(&mut rig, &watch(true, true), then);
    assert!(told_in(ana.heard(then)).is_empty());
    let due = soon + FACTS_EVERY;
    assert_eq!(rig.host.share_deadline(), Some(due));
    rig.tick(due);
    assert_eq!(told_in(ana.heard(due)), ["2 watching, HEVC"]);
    // And that it does not again, as when its GPU refused a stream of
    // it: at once, still no IDR.
    let refused = due + Duration::from_millis(10);
    bo.say(&mut rig, &watch(true, false), refused);
    assert_eq!(told_in(ana.heard(refused)), ["2 watching, H.264"]);

    // Dee, who takes HEVC, starts watching inside FACTS_EVERY: her
    // facts wait, and her ask goes alone.
    let dee_at = refused + Duration::from_millis(10);
    dee.say(&mut rig, &watch(true, true), dee_at);
    assert_eq!(told_in(ana.heard(dee_at)), ["IDR alone"]);

    // Bo stops watching: HEVC again, in the facts' own time.
    let gone = dee_at + Duration::from_millis(10);
    bo.say(&mut rig, &watch(false, false), gone);
    assert!(told_in(ana.heard(gone)).is_empty());
    let due = refused + FACTS_EVERY;
    rig.tick(due);
    assert_eq!(told_in(ana.heard(due)), ["2 watching, HEVC"]);
}

// Video the sharer sent just before Stop sharing arrives after it, over
// a path of its own: it goes nowhere but is not counted as bad. A
// second later it is.
#[test]
fn late_video_after_share_end() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let mut bo = Guest::new();
    bo.join(&mut rig, "Bo", start);
    let share = granted(&mut rig, &mut ana, start);
    bo.say(
        &mut rig,
        &Message::Watch {
            share,
            on: true,
            hevc: true,
        },
        start,
    );
    ana.say(&mut rig, &Message::ShareStop, start);
    let from = ana.wire.addr();
    let packet = sent_video(&mut ana, &tiny_packet());
    rig.deliver(&packet, from, start + Duration::from_millis(100));
    assert_eq!((rig.host.drops.bad, rig.host.screen.relayed), (0, 0));
    let packet = sent_video(&mut ana, &tiny_packet());
    rig.deliver(&packet, from, start + Duration::from_secs(2));
    assert_eq!(rig.host.drops.bad, 1);
}

mod admission;
mod limits;
mod media;
