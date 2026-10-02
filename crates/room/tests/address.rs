// Routers on either side that come back with a new outside address
// mid-session, played on loopback.

mod common;

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use common::{
    FakeNameserver, FakeStun, FakeSystem, Forwarder, Member, NAME, OUTSIDE, fresh_log, lookup,
    loopback, poll, read_log, timers, wake,
};
use invite::{Answers, Candidate, CandidateKind, Invite, ReplyCode};
use keys::Identity;
use room::Timers;
use room::view::{AddressChanged, LinkState, Notice, ReplyState, View};

fn live(v: &View) -> bool {
    v.strip.state == LinkState::Live
}

fn reconnecting(v: &View) -> bool {
    v.strip.state == LinkState::Reconnecting
}

// The host's own code, with `addr` as its one way in from outside.
fn invite_through(host: &Member, addr: SocketAddr) -> Invite {
    let view = host.wait_for(Duration::from_secs(2), "invite code", |v| {
        v.invite.as_ref().is_some_and(|i| !i.code.is_empty())
    });
    let mut invite = Invite::decode(&view.invite.expect("an invite").code).expect("it decodes");
    invite.candidates.push(Candidate {
        kind: CandidateKind::Public,
        addr,
    });
    invite
}

fn contains_all(text: &str, want: &[String]) {
    for line in want {
        assert!(text.contains(line.as_str()), "no {line:?} in\n{text}");
    }
}

// With an address name: the host's router comes back with a new outside
// address and keeps the port, the host's own dynamic DNS client updates the
// name, and the client finds the host there on the session it had.
#[test]
fn client_follows_the_host_through_its_name() {
    let host_log = fresh_log("address", "name", "host");
    let client_log = fresh_log("address", "name", "client");
    let system = FakeSystem::new(Ipv4Addr::LOCALHOST);
    let nameserver = FakeNameserver::start(Ipv4Addr::LOCALHOST);
    let names = lookup(&system, nameserver.port, true);
    let mut config = common::config("Host", timers());
    config.address_name = Some(NAME.to_owned());
    config.lookup = names.clone();
    config.log = Some(host_log.clone());
    let mut host = Member::host_with(config);
    let before = Forwarder::new(loopback(host.port()));
    let invite = invite_through(&host, before.addr);
    assert_eq!(invite.hostname.as_deref(), Some(NAME));
    let mut config = common::config("Ana", timers());
    config.lookup = names;
    config.log = Some(client_log.clone());
    let mut client = Member::join_with(config, Arc::new(Identity::generate()), invite);
    client.wait_for(Duration::from_secs(2), "client live", live);
    // The host's log names the friend.
    host.wait_for(Duration::from_secs(2), "the friend is in", |v| {
        v.people.len() == 2 && v.people[1].name == "Ana"
    });

    // The same port on another address, and nothing at the old one. A real
    // restart outlasts reconnecting_after, so nothing gets through until
    // both sides have noticed the silence; the name already points at the
    // new address. With the path open at once, the client's first ping there
    // could reach the host before the host's own timer had the friend as
    // reconnecting (whenever the host heard the client a little later than
    // the client heard the host), and the host would write no "heard again".
    let port = before.addr.port();
    let old_side = before.host_side();
    drop(before);
    let moved = Ipv4Addr::new(127, 0, 0, 2);
    let after = Forwarder::at(SocketAddr::from((moved, port)), loopback(host.port()));
    after.block(true);
    nameserver.point_to(moved);

    client.wait_for(Duration::from_secs(2), "client reconnecting", reconnecting);
    host.wait_for(
        Duration::from_secs(2),
        "the host has the friend as reconnecting",
        |v| v.people.get(1).is_some_and(|person| person.reconnecting),
    );
    after.block(false);
    let view = client.wait_for(
        Duration::from_secs(2),
        "client live at the new address",
        |v| live(v) && v.numbers.peer_addr == Some(after.addr),
    );
    assert!(after.initiations().is_empty(), "it took a new handshake");
    assert_eq!(view.notice, None);
    let change = view
        .numbers
        .address_change
        .expect("the move is written down");
    assert!(!change.this_pc);
    assert_eq!(change.from, SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
    assert_eq!(change.to, after.addr);
    let reconnect = view.numbers.reconnect_ms.expect("a reconnect time");
    assert!(
        (1000.0..3000.0).contains(&reconnect),
        "{reconnect:.1} ms: the client went reconnecting at 1 s and lost at 3 s"
    );
    println!("reconnect time through the address name: {reconnect:.1} ms");
    let hosted = host.wait_for(Duration::from_secs(1), "the host follows", |v| {
        v.people.len() == 2 && !v.people[1].reconnecting
    });
    assert_eq!(hosted.numbers.peer_addr, Some(after.host_side()));

    client.leave();
    host.leave();
    let at = after.addr;
    contains_all(
        &read_log(&client_log),
        &[
            format!(
                "address name: looking up {NAME} every 2.0 s for up to 60.0 s while the host is silent"
            ),
            format!("address name: trying {at} too"),
            format!("the host moved from 127.0.0.1:{port} to {at}"),
            format!("the host is heard again at {at}, after "),
        ],
    );
    contains_all(
        &read_log(&host_log),
        &[
            format!("moved from {old_side} to {}", after.host_side()),
            String::from("\"Ana\": heard again after "),
        ],
    );
}

// Without a name: the host's router comes back with a new outside address
// and port. The client has no way to learn it, but the host pings every
// friend from the new one as soon as STUN shows it, and a friend whose router
// lets that in follows by the roaming rule.
#[test]
fn client_follows_the_hosts_ping_from_a_new_address() {
    let host_log = fresh_log("address", "open", "host");
    let seen = Arc::new(Mutex::new(OUTSIDE));
    // Called by the STUN server as the host's question comes in.
    type BackUp = Arc<Mutex<Option<Box<dyn Fn() + Send>>>>;
    let back_up: BackUp = Arc::new(Mutex::new(None));
    let stun = FakeStun::answering(Some(Duration::ZERO), {
        let (seen, back_up) = (Arc::clone(&seen), Arc::clone(&back_up));
        move |_| {
            if let Some(back_up) = back_up.lock().unwrap().take() {
                back_up();
            }
            *seen.lock().unwrap()
        }
    });
    // The host's own pings come every 10 s. It notices after the client
    // has gone reconnecting, so the client has a reconnect time.
    let slow = Timers {
        ping_idle: Duration::from_secs(10),
        reconnecting_after: Duration::from_secs(2),
        lost_after: Duration::from_secs(6),
        ..timers()
    };
    let mut config = common::config("Host", slow);
    config.stun_servers = vec![stun.addr.to_string()];
    config.log = Some(host_log.clone());
    let mut host = Member::host_with(config);
    let router = Forwarder::new(loopback(host.port()));
    let mut client = Member::join("Ana", timers(), invite_through(&host, router.addr));
    client.wait_for(Duration::from_secs(2), "client live", live);
    host.wait_for(Duration::from_secs(2), "the friend is in", |v| {
        v.people.len() == 2 && v.numbers.public_addr == Some(SocketAddr::V4(OUTSIDE))
    });

    // Everything the host sent on joining is acknowledged by now, so it has
    // nothing to send again later.
    client.holds_for(Duration::from_millis(500), "the client stays", live);

    // The router restarts and comes back with another outside address and
    // port, just as the host asks STUN whether its address changed. Until
    // then everything is dropped, the rosters the host keeps sending
    // included, so the first packet through the new port toward the client
    // is the ping the host sends on the answer. What the client sends to
    // the old port goes nowhere.
    let new_outside = SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 10), 52100);
    *seen.lock().unwrap() = new_outside;
    router.block(true);
    let moved_to = router.move_outside();
    *back_up.lock().unwrap() = Some(Box::new(router.unblocker()));
    let hosted = host.wait_for(
        Duration::from_secs(4),
        "the host sees its new address",
        |v| v.numbers.address_change.is_some(),
    );
    let change = hosted.numbers.address_change.expect("the change");
    assert!(change.this_pc);
    assert_eq!(change.from, SocketAddr::V4(OUTSIDE));
    assert_eq!(change.to, SocketAddr::V4(new_outside));
    assert_eq!(
        hosted.numbers.public_addr,
        Some(SocketAddr::V4(new_outside))
    );
    assert!(
        hosted
            .invite
            .as_ref()
            .is_some_and(|invite| invite.address_changed_since)
    );
    assert_eq!(
        hosted.address_changed,
        Some(AddressChanged {
            codes_cannot_help: true,
            friends_lost: false,
            name_set: false,
        })
    );

    let view = client.wait_for(
        Duration::from_millis(500),
        "client live at the host's new address",
        |v| live(v) && v.numbers.peer_addr == Some(moved_to),
    );
    let change = view
        .numbers
        .address_change
        .expect("the move is written down");
    assert!(!change.this_pc);
    assert_eq!((change.from, change.to), (router.addr, moved_to));
    let reconnect = view.numbers.reconnect_ms.expect("a reconnect time");
    println!("reconnect time after the host's ping: {reconnect:.1} ms");
    // The host noticed at its reconnecting_after, and the ping went at once.
    // Nothing else would have been this quick: the roster the host sent
    // into the outage goes again only as its backed off timer says, up to
    // 2 s apart. The client counts from the last answer it heard and the
    // host from the last ping, which is one of the client's pings later
    // when that ping got through and its answer did not.
    let noticed = slow.reconnecting_after.as_secs_f32() * 1000.0;
    let one_ping = timers().ping_idle.as_secs_f32() * 1000.0;
    assert!(
        reconnect < noticed + one_ping + 50.0,
        "{reconnect:.1} ms, the host noticed at {noticed:.0} ms"
    );
    let hosted = host.wait_for(Duration::from_secs(1), "the friend is back", |v| {
        v.people.len() == 2 && !v.people[1].reconnecting && v.numbers.reconnect_ms.is_some()
    });
    assert_eq!(hosted.address_changed, None);

    client.leave();
    host.leave();
    contains_all(
        &read_log(&host_log),
        &[
            String::from(
                "every friend went quiet within 1.0 s of each other, asking stun whether the public address changed",
            ),
            format!("this pc's public address changed from {OUTSIDE} to {new_outside}"),
            String::from(
                "the invite on show was made before the change, its outside addresses lead nowhere now",
            ),
            String::from("pinged 1 friend at their last addresses from the new one: "),
        ],
    );
}

// A strict router in front of the host, with no port mapped. Every outside
// source gets an inside socket of its own, which is where the host sees
// that source, and nothing from a source gets in before the host has sent
// to its inside socket.
struct StrictRouter {
    outside: SocketAddr,
    shared: Arc<Strict>,
    thread: Option<JoinHandle<()>>,
}

struct Strict {
    stop: AtomicBool,
    outside: UdpSocket,
    host: SocketAddr,
    sources: Mutex<Vec<Source>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

struct Source {
    from: SocketAddr,
    inside: Arc<UdpSocket>,
    open: Arc<AtomicBool>,
}

fn router_socket() -> UdpSocket {
    UdpSocket::bind(loopback(0)).expect("bind a router socket")
}

impl StrictRouter {
    fn new(host: SocketAddr) -> StrictRouter {
        let shared = Arc::new(Strict {
            stop: AtomicBool::new(false),
            outside: router_socket(),
            host,
            sources: Mutex::new(Vec::new()),
            threads: Mutex::new(Vec::new()),
        });
        let outside = shared.outside.local_addr().expect("local addr");
        let thread = thread::spawn({
            let shared = Arc::clone(&shared);
            move || {
                let mut buf = [0u8; 2048];
                while !shared.stop.load(Ordering::Acquire) {
                    let Ok((len @ 1.., from)) = shared.outside.recv_from(&mut buf) else {
                        continue;
                    };
                    let (inside, open) = source(&shared, from);
                    if open.load(Ordering::Acquire) {
                        let _ = inside.send_to(&buf[..len], shared.host);
                    }
                }
            }
        });
        StrictRouter {
            outside,
            shared,
            thread: Some(thread),
        }
    }

    // As if the host had sent there already.
    fn let_in(&self, from: SocketAddr) {
        source(&self.shared, from).1.store(true, Ordering::Release);
    }

    fn inside_for(&self, from: SocketAddr) -> Option<SocketAddr> {
        let sources = self.shared.sources.lock().unwrap();
        let source = sources.iter().find(|source| source.from == from)?;
        source.inside.local_addr().ok()
    }
}

// The inside socket and the open flag for packets from `from`, made on
// first sight, with a thread that carries the host's packets back out.
fn source(shared: &Arc<Strict>, from: SocketAddr) -> (Arc<UdpSocket>, Arc<AtomicBool>) {
    let mut sources = shared.sources.lock().unwrap();
    if let Some(known) = sources.iter().find(|source| source.from == from) {
        return (Arc::clone(&known.inside), Arc::clone(&known.open));
    }
    let inside = Arc::new(router_socket());
    let open = Arc::new(AtomicBool::new(false));
    let going_out = thread::spawn({
        let (shared, inside, open) = (Arc::clone(shared), Arc::clone(&inside), Arc::clone(&open));
        move || {
            let mut buf = [0u8; 2048];
            while !shared.stop.load(Ordering::Acquire) {
                let Ok((len @ 1.., sender)) = inside.recv_from(&mut buf) else {
                    continue;
                };
                if sender == shared.host {
                    open.store(true, Ordering::Release);
                    let _ = shared.outside.send_to(&buf[..len], from);
                }
            }
        }
    });
    shared.threads.lock().unwrap().push(going_out);
    sources.push(Source {
        from,
        inside: Arc::clone(&inside),
        open: Arc::clone(&open),
    });
    (inside, open)
}

// Once the outside thread is gone no new source can come, so every inside
// socket left is woken.
impl Drop for StrictRouter {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        wake(&self.shared.outside);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        for source in self.shared.sources.lock().unwrap().iter() {
            wake(&source.inside);
        }
        for thread in self.shared.threads.lock().unwrap().drain(..) {
            let _ = thread.join();
        }
    }
}

fn has_code(v: &View) -> bool {
    v.reply
        .as_ref()
        .is_some_and(|reply| matches!(reply.state, ReplyState::Code { .. }))
}

// The host needed no code the first time only because its router was told
// to let this friend in. After the friend's router comes back on another
// port, only a code gets them back. STUN cannot answer the round the silence
// starts while that router is still down, so the client asks again until it
// does.
#[test]
fn moved_client_gets_back_in_with_a_code() {
    let host_log = fresh_log("address", "code", "host");
    let client_log = fresh_log("address", "code", "client");
    let patient = Timers {
        lost_after: Duration::from_secs(6),
        ..timers()
    };
    let mut config = common::config("Host", patient);
    config.log = Some(host_log.clone());
    let mut host = Member::host_with(config);
    let strict = StrictRouter::new(loopback(host.port()));
    let seen = Arc::new(Mutex::new(OUTSIDE));
    let stun = FakeStun::answering(Some(Duration::ZERO), {
        let seen = Arc::clone(&seen);
        move |_| *seen.lock().unwrap()
    });
    let front = Forwarder::new(strict.outside);
    strict.let_in(front.host_side());
    let mut config = common::config("Ana", patient);
    config.stun_servers = vec![stun.addr.to_string()];
    config.log = Some(client_log.clone());
    let identity = Arc::new(Identity::generate());
    let invite = invite_through(&host, front.addr);
    let mut client = Member::join_with(config, Arc::clone(&identity), invite);
    // The roster comes a moment after the first packet on the session, and
    // the router going down must not cut it off: the people list is where
    // the code shows.
    client.wait_for(Duration::from_secs(2), "client live with the roster", |v| {
        live(v) && v.people.len() == 2
    });
    host.wait_for(Duration::from_secs(2), "the friend is in", |v| {
        v.people.len() == 2 && v.people[1].name == "Ana"
    });
    poll(
        Duration::from_secs(2),
        "the first stun round answered",
        || (stun.answered() >= 1).then_some(()),
    );
    let joined_with = front.initiations().len();
    let was_inside = strict
        .inside_for(front.host_side())
        .expect("the router let the friend in");

    // The friend's router restarts. Nothing gets through it, STUN included,
    // until the client has gone reconnecting and asked STUN once.
    stun.silence(true);
    front.block(true);
    client.wait_for(Duration::from_secs(2), "client reconnecting", reconnecting);
    poll(Duration::from_secs(2), "stun asked in the silence", || {
        (stun.requests() >= 2).then_some(())
    });
    assert!(!has_code(&client.view()), "a code with no answer from stun");

    // It comes back on another outside port, which the host's router has
    // never seen.
    let moved = SocketAddrV4::new(*OUTSIDE.ip(), OUTSIDE.port() + 1);
    *seen.lock().unwrap() = moved;
    let now_from = front.rebind();
    front.block(false);
    stun.silence(false);

    let view = client.wait_for(
        Duration::from_secs(2),
        "the code above the people list",
        |v| reconnecting(v) && !v.people.is_empty() && has_code(v),
    );
    assert!(stun.requests() >= 3, "the code came without asking again");
    let change = view.numbers.address_change.expect("the change");
    assert!(change.this_pc);
    assert_eq!(change.from, SocketAddr::V4(OUTSIDE));
    assert_eq!(change.to, SocketAddr::V4(moved));
    let mut code = ReplyCode::decode(&view.reply.expect("the code").code).expect("it decodes");
    assert_eq!(code.answers, Answers::Rejoin);
    assert_eq!(code.client_key, *identity.public());
    assert_eq!(code.outside_v4, Some(moved));

    host.wait_for(
        Duration::from_secs(2),
        "the host has the friend as reconnecting",
        |v| v.people.get(1).is_some_and(|person| person.reconnecting),
    );
    // The text format refuses loopback, so the struct gets the address the
    // host's side sees the friend's new port at, as in reply.rs.
    let inside = poll(
        Duration::from_secs(2),
        "the router saw the new port",
        || strict.inside_for(now_from),
    );
    let SocketAddr::V4(inside_v4) = inside else {
        panic!("the router is on IPv4 loopback");
    };
    code.outside_v4 = Some(inside_v4);
    let accepted = host
        .room()
        .accept_reply(code)
        .expect("the paste is accepted");
    assert_eq!(accepted.to, [inside]);

    let back = client.wait_for(Duration::from_secs(2), "client live again", live);
    assert_eq!(back.reply, None);
    assert_eq!(back.notice, None);
    assert!(back.numbers.reconnect_ms.is_some());
    assert_eq!(
        front.initiations().len(),
        joined_with,
        "it took a new handshake"
    );
    let hosted = host.wait_for(Duration::from_secs(1), "the friend is back", |v| {
        v.people.len() == 2 && !v.people[1].reconnecting
    });
    assert_eq!(hosted.numbers.peer_addr, Some(inside));

    client.leave();
    host.leave();
    let fingerprint = identity.fingerprint();
    contains_all(
        &read_log(&client_log),
        &[
            String::from("asking stun whether this pc's outside address changed"),
            String::from("stun: no server answered, asking again in "),
            format!("this pc's outside address changed from {OUTSIDE} to {moved}"),
            String::from("the code shows above the people list, for the host to paste"),
            String::from("the code above the people list goes, the host answered"),
        ],
    );
    contains_all(
        &read_log(&host_log),
        &[
            format!("reply code pasted for {fingerprint}: answers a rejoin, outside {inside}"),
            format!("punch round 1 of 10 for {fingerprint} sent to {inside}"),
            format!("\"Ana\": moved from {was_inside} to {inside}"),
        ],
    );
}

// Longer than lost_after: the host let the friend go, and the client's
// ladder brings it back with the per-peer secret, which the host keeps for
// every key that was in the room this run.
#[test]
fn lost_client_comes_back_with_its_secret() {
    let host_log = fresh_log("address", "lost", "host");
    let mut config = common::config("Host", timers());
    config.log = Some(host_log.clone());
    let mut host = Member::host_with(config);
    let router = Forwarder::new(loopback(host.port()));
    let mut client = Member::join("Ana", timers(), invite_through(&host, router.addr));
    client.wait_for(Duration::from_secs(2), "client live", live);
    host.wait_for(Duration::from_secs(2), "the friend is in", |v| {
        v.people.len() == 2
    });

    router.block(true);
    client.wait_for(Duration::from_secs(5), "client lost", |v| {
        v.strip.state == LinkState::Lost && v.notice == Some(Notice::LostHost)
    });
    host.wait_for(Duration::from_secs(2), "the host let the friend go", |v| {
        v.people.len() == 1
    });
    router.block(false);

    let view = client.wait_for(Duration::from_secs(3), "client back", |v| {
        live(v) && v.notice.is_none() && v.people.len() == 2
    });
    let reconnect = view.numbers.reconnect_ms.expect("a reconnect time");
    assert!(reconnect >= 3000.0, "{reconnect:.1} ms");
    println!("reconnect time after being let go: {reconnect:.1} ms");
    let hosted = host.wait_for(Duration::from_secs(1), "the friend is back", |v| {
        v.people.len() == 2 && v.numbers.reconnect_ms.is_some()
    });
    assert!(hosted.numbers.reconnect_ms.expect("a reconnect time") >= 3000.0);

    client.leave();
    host.leave();
    let fingerprint = client.identity.fingerprint();
    contains_all(
        &read_log(&host_log),
        &[
            format!("{fingerprint}: session confirmed (known)"),
            format!("{fingerprint}: back after "),
        ],
    );
}

// The address watch hands every Windows notification to the room, and
// Windows sends them in bursts. A burst asks STUN one round, not a hundred.
#[test]
fn a_burst_of_address_changes_asks_stun_once() {
    let stun = FakeStun::start(Some(Duration::from_millis(300)));
    // A retry inside the hold below would count as a second round.
    let slow = Timers {
        stun_wait: Duration::from_secs(1),
        stun_retry: Duration::from_secs(2),
        ..timers()
    };
    let mut config = common::config("Host", slow);
    config.stun_servers = vec![stun.addr.to_string()];
    let mut host = Member::host_with(config);
    host.wait_for(Duration::from_secs(2), "the first round answered", |v| {
        v.numbers.public_addr.is_some()
    });
    assert_eq!(stun.requests(), 1);

    // Spread over 200 ms, less than the 300 ms the answer takes, so most of
    // them reach the room one at a time while the round is out.
    let hook = host.room().address_hook();
    let began = Instant::now();
    for i in 0..100 {
        hook.changed();
        let next = began + Duration::from_millis(2) * (i + 1);
        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            thread::sleep(wait);
        }
    }
    host.holds_for(
        Duration::from_millis(1200),
        "the room goes on through the burst",
        |v| v.invite.is_some() && v.notice.is_none(),
    );
    assert_eq!(stun.requests(), 2, "one round for the whole burst");
    host.leave();
    // A hook that outlives its room does nothing.
    hook.changed();
}

// The app turns the room's own watch on in both roles. A room is opened and
// left many times in one run, and leave must still take under 200 ms with
// Windows' registration to cancel on the way out.
#[test]
fn address_watch_comes_and_goes_with_the_room() {
    let host_log = fresh_log("address", "watch", "host");
    let client_log = fresh_log("address", "watch", "client");
    for round in 0..10 {
        let mut config = common::config("Host", timers());
        config.watch_addresses = true;
        config.log = (round == 0).then(|| host_log.clone());
        let mut host = Member::host_with(config);
        let mut config = common::config("Ana", timers());
        config.watch_addresses = true;
        config.log = (round == 0).then(|| client_log.clone());
        let invite = common::host_invite(&host);
        let mut client = Member::join_with(config, Arc::new(Identity::generate()), invite);
        client.wait_for(Duration::from_secs(2), "client live", live);
        for (who, member) in [("client", &mut client), ("host", &mut host)] {
            let took = member.leave();
            assert!(
                took < Duration::from_millis(200),
                "round {round}: {who} leave took {took:?}"
            );
        }
    }
    for (who, log) in [("host", &host_log), ("client", &client_log)] {
        let text = read_log(log);
        assert!(
            text.contains("watching this pc's addresses, a change asks stun at once"),
            "{who} did not start the watch:\n{text}"
        );
    }
}
