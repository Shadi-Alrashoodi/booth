// Wrong addresses and races in the STUN questions.

use super::*;

// A server that says it sees this PC on its own network, on loopback, on
// port 0 or as IPv4 written as IPv6 is broken or lying. Nothing it says is
// kept, and the question stays open for the real answer.
#[test]
fn an_answer_from_inside_is_not_taken() {
    let socket = Socket::bind(0, Log::off()).expect("bind");
    let server = Wire::new();
    let start = Instant::now();
    let mut stun = Stun::new(None, Duration::from_millis(100), RETRY, start, Log::off());
    stun.found(vec![server.addr()], &socket);
    assert!(!stun.resolved(start));
    for inside in [
        "192.168.1.20:52000",
        "127.0.0.1:52000",
        "100.64.0.1:52000",
        "203.0.113.9:0",
        "[::ffff:203.0.113.9]:52000",
        "[fd00::1]:52000",
    ] {
        let seen: SocketAddr = inside.parse().unwrap();
        let answer = answer_seeing(&mut stun, server.addr(), seen, start);
        assert!(matches!(answer, Answer::NotOurs(_)), "{inside} was taken");
        assert_eq!(stun.public(), None, "after {inside}");
        assert!(!stun.is_settled(), "settled on {inside}");
    }
    let real = answer_with(&mut stun, server.addr(), outside(52000), start);
    assert!(matches!(real, Answer::Settled));
    assert_eq!(stun.public_v4(), Some(outside(52000)));
    assert_eq!(stun.public_v6(), None);
}

// A keepalive round leaves just before Windows reports an address change
// and comes back the old way, with the old address. It went out before the
// change, so it cannot end the check: another round goes at stun_retry.
#[test]
fn a_round_out_before_the_change_does_not_end_the_check() {
    let socket = Socket::bind(0, Log::off()).expect("bind");
    let server = Wire::new();
    let start = Instant::now();
    let every = Duration::from_secs(20);
    let wait = Duration::from_millis(1500);
    let mut stun = Stun::new(Some(every), wait, RETRY, start, Log::off());
    stun.found(vec![server.addr()], &socket);
    assert!(!stun.resolved(start));
    answer_with(&mut stun, server.addr(), outside(52000), start);
    assert!(stun.is_settled());
    server.packets();

    let keepalive = start + every;
    stun.tick(keepalive, &socket);
    assert_eq!(server.packets().len(), 1);
    let noticed = keepalive + Duration::from_millis(20);
    assert!(!stun.check(noticed, &socket), "the round out goes on");
    let old = answer_with(
        &mut stun,
        server.addr(),
        outside(52000),
        noticed + Duration::from_millis(10),
    );
    assert!(matches!(old, Answer::Recorded));
    assert!(stun.check_until.is_some(), "the old answer ended the check");
    assert_eq!(
        stun.next_deadline(),
        Some(keepalive + RETRY),
        "no retry at stun_retry"
    );
    stun.tick(keepalive + RETRY, &socket);
    assert_eq!(server.packets().len(), 1, "asked again after the change");
}

// Only the first server answered in time, so the mapping is not known, and
// the router shows each server its own port. After that each server only
// repeats itself, and the outside address stays put rather than following
// whichever server answered last.
#[test]
fn a_server_repeating_itself_moves_nothing() {
    let socket = Socket::bind(0, Log::off()).expect("bind");
    let (one, two) = (Wire::new(), Wire::new());
    let start = Instant::now();
    let every = Duration::from_secs(20);
    let wait = Duration::from_millis(100);
    let mut stun = Stun::new(Some(every), wait, RETRY, start, Log::off());
    stun.found(vec![one.addr(), two.addr()], &socket);
    assert!(!stun.resolved(start));
    answer_with(&mut stun, one.addr(), outside(52000), start);
    assert!(stun.tick(start + wait, &socket));
    assert_eq!(stun.mapping, Mapping::Unknown);
    let late = answer_with(&mut stun, two.addr(), outside(52007), start + wait);
    assert!(matches!(late, Answer::Recorded));
    let kept = stun.public_v4();

    let mut at = start;
    for _ in 0..3 {
        at += every;
        stun.tick(at, &socket);
        for (server, port) in [(&one, 52000), (&two, 52007)] {
            let answer = answer_with(&mut stun, server.addr(), outside(port), at);
            assert!(matches!(answer, Answer::Recorded));
            assert_eq!(stun.public_v4(), kept, "after {port}");
        }
    }
}
