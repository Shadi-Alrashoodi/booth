// Who an invite lets in, and who holds a seat while a handshake is under
// way. A reply code that answers an invite carries its id, and only the
// invite carries its secret. IKpsk2 mixes the psk in at the end of message
// 2, so the host answers message 1 before it can tell whether the client
// had the secret.

use super::*;

// Message 1 naming the invite on show, with a psk made up for want of the
// secret.
fn knock_without_secret(guest: &mut Guest, rig: &mut Rig, from: SocketAddr, now: Instant) {
    let kind = InitKind::Invite(rig.invite.invite_id);
    guest.initiate(rig, kind, &[0x5a; 32], from, now);
}

fn hello(name: &str) -> Message {
    Message::Hello {
        version: invite::VERSION,
        name: name.to_owned(),
        reached: None,
    }
}

#[test]
fn invite_id_alone_does_not_spend_a_single_use_invite() {
    let start = Instant::now();
    let (mut host, socket) = Rig::host(start, Log::off(), false);
    host.stun_resolved(start);
    host.new_invite(false, start);
    let mut rig = Rig::holding(host, socket, start);

    // Eve has the id from a reply code and makes up the rest.
    let mut eve = Guest::new();
    let from = eve.wire.addr();
    knock_without_secret(&mut eve, &mut rig, from, start);
    assert_eq!(eve.wire.packets().len(), 1, "message 1 is answered");

    // Ana has the invite itself, and it is still hers to use.
    let mut ana = Guest::new();
    let knock = ana.knock(&mut rig, start);
    let answered = ana.answer(knock, start);
    ana.session = Some(answered.expect("Ana was refused, the invite went to Eve's key"));
    ana.say(&mut rig, &hello("Ana"), start);
    assert_eq!(rig.host.peers.len(), 1);
    assert_eq!(rig.host.peers[0].key, *ana.identity.public());
}

#[test]
fn invite_id_alone_does_not_fill_a_multi_use_invite() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    assert!(rig.host.view(start, 0).invite.expect("an invite").multi_use);

    // As many made-up keys as a multi-use invite lets in, each after the
    // last one's answer ran out, so none finds the room full.
    let eve = Wire::at(Ipv4Addr::new(127, 0, 7, 1));
    let mut now = start;
    for _ in 0..32 {
        now += PENDING_LIFETIME + Duration::from_secs(1);
        rig.tick(now);
        knock_without_secret(&mut Guest::new(), &mut rig, eve.addr(), now);
        assert_eq!(eve.packets().len(), 1, "message 1 is answered");
    }

    // Someone holding the invite still gets in.
    now += Duration::from_secs(1);
    let mut ana = Guest::new();
    let knock = ana.knock(&mut rig, now);
    let answered = ana.answer(knock, now);
    ana.session = Some(answered.expect("Ana was refused, the made-up keys used up the invite"));
    ana.say(&mut rig, &hello("Ana"), now);
    assert_eq!(rig.host.peers.len(), 1);
    assert_eq!(rig.host.peers[0].key, *ana.identity.public());
}

// As many made-up keys as the room has seats keep knocking, once a second
// each from one address, well within its rate limit. The host answers them
// all, and they must not hold the seats a known device coming back and a
// friend with the invite need.
#[test]
fn made_up_keys_hold_no_seats() {
    let start = Instant::now();
    let mut bo = Guest::new();
    let devices = KnownDevices {
        devices: vec![known::KnownDevice {
            key: *bo.identity.public(),
            name: String::from("Bo"),
            first_seen: 1_790_000_000,
            last_seen: 1_790_000_000,
            secret: Zeroizing::new([4; 32]),
        }],
        blocked: Vec::new(),
    };
    let mut rig = Rig::reopened(start, Arc::new(Identity::generate()), devices, Log::off());

    let eve = Wire::at(Ipv4Addr::new(127, 0, 7, 2));
    let mut made_up: Vec<Guest> = (0..MAX_CLIENTS).map(|_| Guest::new()).collect();
    let mut now = start;
    for _ in 0..10 {
        now += Duration::from_secs(1);
        for guest in &mut made_up {
            knock_without_secret(guest, &mut rig, eve.addr(), now);
        }
        rig.tick(now);
        assert_eq!(eve.packets().len(), MAX_CLIENTS, "every try is answered");
    }

    let from = bo.wire.addr();
    let rejoin = bo.initiate(&mut rig, InitKind::Known, &[4; 32], from, now);
    assert!(bo.answer(rejoin, now).is_some(), "Bo found the room full");
    let mut ana = Guest::new();
    let knock = ana.knock(&mut rig, now);
    assert!(ana.answer(knock, now).is_some(), "Ana found the room full");
}

// Stamps are not kept between runs. A known device's initiation from long
// before the room opened, kept by someone on the path, is not answered
// when it is sent again after a restart.
#[test]
fn initiation_from_before_restart_is_not_answered() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let save = rig.host.take_last_save().expect("the join is saved");
    let saved = known::parse_devices(&save.bytes).expect("what was saved reads back");
    let secret = rig.host.keys[ana.identity.public()]
        .secret
        .clone()
        .expect("a secret");

    let two_days_ago = std::time::SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60);
    let (_, old) = Initiation::start(
        &ana.identity.private_bytes(),
        ana.identity.public(),
        &rig.invite.host_key,
        &secret,
        InitKind::Known,
        Tai64N::from_system_time(two_days_ago),
        1,
    )
    .expect("start an initiation");

    let identity = Arc::clone(&rig.host.identity);
    let later = start + Duration::from_secs(60);
    rig.host.leave(later, &rig.socket);
    let mut again = Rig::reopened(later, identity, saved, Log::off());
    ana.left();
    let path = Wire::at(Ipv4Addr::new(127, 0, 7, 3));
    again.deliver(&old, path.addr(), later);
    assert!(path.packets().is_empty(), "the old one was answered");
    assert!(again.host.pending.is_empty());

    // Ana herself, with a stamp from now, is answered.
    let from = ana.wire.addr();
    let rejoin = ana.initiate(&mut again, InitKind::Known, &secret, from, later);
    assert!(ana.answer(rejoin, later).is_some(), "Ana was not answered");
}
