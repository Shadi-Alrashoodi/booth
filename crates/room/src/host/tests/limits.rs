// What an initiation spends from the limits in limit.rs before any key math
// or cookie reply, and what that must leave for a friend in the room.

use super::*;

// How many sources limit.rs keeps a bucket for, its MAX_SOURCES.
const MAX_SOURCES: u32 = 4096;

fn secret_of(rig: &Rig, guest: &Guest) -> Zeroizing<[u8; 32]> {
    rig.host.keys[guest.identity.public()]
        .secret
        .clone()
        .expect("the host made a secret on the join")
}

// The cookie in the host's reply to `tried`, if one came.
fn cookie_for(rig: &Rig, guest: &Guest, tried: &Initiation) -> Option<[u8; 16]> {
    guest.wire.packets().iter().find_map(|packet| {
        session::read_cookie_reply(packet, &rig.invite.host_key, &tried.mac1())
            .ok()
            .map(|(_, cookie)| cookie)
    })
}

// Under load Ana rekeys from where she is in the room, first without a
// cookie. She gets one, and the rekey that returns it is answered.
fn rekey_under_load(rig: &mut Rig, ana: &mut Guest, now: Instant) {
    let secret = secret_of(rig, ana);
    let from = ana.wire.addr();
    ana.wire.packets();
    let first = ana.initiate(rig, InitKind::Rekey, &secret, from, now);
    let cookie = cookie_for(rig, ana, &first).expect("a cookie reply to Ana's rekey");
    let again = ana.initiate_with(rig, InitKind::Rekey, &secret, from, Some(&cookie), now);
    assert!(
        ana.answer(again, now).is_some(),
        "with the cookie the rekey is answered"
    );
}

// Without mac1 an initiation is dropped before any key math, so it must
// cost nothing. One byte from each of more addresses than the shared budget
// has tokens must not use it up for every join.
#[test]
fn keyless_junk_spends_no_join_budget() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let bad = rig.host.drops.bad;
    for n in 1..=60 {
        rig.deliver(&[0x11], SocketAddr::from(([127, 0, 9, n], 9)), start);
    }
    assert_eq!(rig.host.drops.bad, bad + 60);
    assert_eq!(reads(&rig), 0);

    let mut bo = Guest::new();
    let tried = bo.knock(&mut rig, start);
    assert!(
        bo.answer(tried, start).is_some(),
        "the junk used up the joins"
    );
}

// Friends behind one NAT share its address, and anyone can send from it.
// Junk from another port there must not use up a friend's rekey.
#[test]
fn junk_from_a_friends_ip_keeps_its_rekey() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);
    let secret = secret_of(&rig, &ana);
    let from = ana.wire.addr();
    let next_door = SocketAddr::new(from.ip(), 9);

    let now = start + Duration::from_secs(1);
    let bad = rig.host.drops.bad;
    for _ in 0..30 {
        rig.deliver(&[0x11], next_door, now);
    }
    assert_eq!(rig.host.drops.bad, bad + 30);
    let rekey = ana.initiate(&mut rig, InitKind::Rekey, &secret, from, now);
    assert!(
        ana.answer(rekey, now).is_some(),
        "junk from {next_door} used up the rekey"
    );
    // Key math for the join and the rekey, none for the junk.
    assert_eq!(reads(&rig), 2);
}

// Under load the same for initiations made for this host's key. Sent from
// another port at a friend's address, they must not use up the cookie reply
// her rekey needs.
#[test]
fn spoofed_cookie_path_keeps_a_friends_rekey() {
    let start = Instant::now();
    let mut rig = Rig::new(start);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);

    let now = flood(&mut rig, start + Duration::from_secs(1));
    let packet = stranger_packet(&rig);
    // A socket of its own at Ana's address, so the replies have somewhere
    // to go.
    let next_door = Wire::new();
    for _ in 0..30 {
        rig.deliver(&packet, next_door.addr(), now);
    }
    // The address's whole burst went to them.
    assert_eq!(cookie_replies(&next_door.packets()), 20);
    rekey_under_load(&mut rig, &mut ana, now);
}

// A flood from many addresses that keeps the cookie replies at their cap
// holds back new joins, not the cookie a friend in the room needs for a
// rekey.
#[test]
fn cookie_cap_keeps_a_friends_rekey() {
    let start = Instant::now();
    let timers = Timers {
        cookie_replies_per_second: 100,
        cookie_reply_burst: 20,
        ..Timers::default()
    };
    let mut rig = Rig::with_timers(start, Log::off(), timers);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);

    let now = flood(&mut rig, start + Duration::from_secs(1));
    let packet = stranger_packet(&rig);
    // Each a socket, so the replies have somewhere to go.
    let sources: Vec<Wire> = (1..=40)
        .map(|n| Wire::at(Ipv4Addr::new(127, 0, 7, n)))
        .collect();
    for source in &sources {
        rig.deliver(&packet, source.addr(), now);
    }
    let stranger = Wire::at(Ipv4Addr::new(127, 0, 7, 41));
    rig.deliver(&packet, stranger.addr(), now);
    assert_eq!(cookie_replies(&stranger.packets()), 0, "the cap is used up");
    rekey_under_load(&mut rig, &mut ana, now);
}

// Under load one initiation leaves a source's bucket short of full for a
// while, so MAX_SOURCES sources at once fill the table limit.rs keeps. Ana
// has been quiet, so the sweep that makes room drops her bucket, and no
// sweep is due again for 100 ms. She must still get the cookie for a rekey.
#[test]
fn full_source_table_keeps_a_friends_rekey() {
    let start = Instant::now();
    // The flood takes the one cookie reply the cap holds, so none go out to
    // the made-up sources below. It holds one again by the time Ana asks.
    let timers = Timers {
        cookie_reply_burst: 1,
        ..Timers::default()
    };
    let mut rig = Rig::with_timers(start, Log::off(), timers);
    let mut ana = Guest::new();
    ana.join(&mut rig, "Ana", start);

    let now = flood(&mut rig, start + Duration::from_secs(1));
    let packet = stranger_packet(&rig);
    // From 127.16.0.0 on.
    for n in 0..MAX_SOURCES {
        let from = SocketAddr::from((Ipv4Addr::from(0x7F10_0000 + n), 9));
        rig.deliver(&packet, from, now);
    }
    assert_eq!(rig.host.cookies.replies_sent, 1, "only the flood's");

    // The cap has a reply again, so the table is what keeps a newcomer out.
    let later = now + Duration::from_millis(50);
    let stranger = Wire::at(Ipv4Addr::new(127, 0, 8, 1));
    rig.deliver(&packet, stranger.addr(), later);
    assert_eq!(cookie_replies(&stranger.packets()), 0, "the table is full");
    rekey_under_load(&mut rig, &mut ana, later);
}
