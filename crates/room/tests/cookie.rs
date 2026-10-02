mod common;

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use common::{
    Flooder, Member, Stranger, config, fresh_log, host_invite, loopback, poll, read_log, timers,
};
use keys::Identity;
use room::view::LinkState;

fn sleep_until(at: Instant) {
    thread::sleep(at.saturating_duration_since(Instant::now()));
}

// Someone who once saw an invite has the host key and can send initiations
// that pass mac1. Under load the host answers those with a cookie reply and
// does no key math until the cookie comes back in mac2.
//
// Every source here is an address of its own on loopback: the rate limit
// counts by address, and the friend must not share a bucket with the flood.
#[test]
fn flood_costs_no_key_math() {
    let host = Member::host("Host", timers());
    let invite = host_invite(&host);
    let host_addr = loopback(host.port());
    let reads = || host.room().initiations_read();
    let stranger = Arc::new(Identity::generate());

    // 8 addresses with 4 ports each, 320 initiations a second in all: 40
    // from each address, of which the rate limit lets 10 have a cookie
    // reply once the first 20 have had theirs.
    let ips: Vec<Ipv4Addr> = (2..10).map(|n| Ipv4Addr::new(127, 0, 0, n)).collect();
    let mut flood = Flooder::start(&stranger, host_addr, &invite.host_key, &ips, 4, 320);
    poll(
        Duration::from_secs(2),
        "cookie replies to the flood",
        || (flood.cookie_replies() > 0).then_some(()),
    );
    // Only what came in before the count reached the threshold was read.
    let under_load = reads();
    assert!(
        under_load < 32,
        "{under_load} initiations read before the host went under load"
    );
    // The flood alone for a while, so what is read later is the friend's.
    let replies = flood.cookie_replies();
    thread::sleep(Duration::from_millis(500));
    assert!(
        flood.cookie_replies() > replies,
        "the host stopped answering the flood"
    );
    assert_eq!(
        reads(),
        under_load,
        "the flood got key math while the host was under load"
    );

    let log = fresh_log("cookie", "flood", "client");
    let mut ana_config = config("Ana", timers());
    ana_config.log = Some(log.clone());
    let started = Instant::now();
    let ana = Member::join_with(ana_config, Arc::new(Identity::generate()), invite.clone());
    ana.wait_for(Duration::from_secs(2), "the friend joins", |v| {
        v.strip.state == LinkState::Live
    });
    println!("joined under load in {:?}", started.elapsed());
    // One try with the cookie, or one per path it took.
    let joined = reads();
    assert!(
        (1..=3).contains(&(joined - under_load)),
        "{} initiations read while the friend joined",
        joined - under_load
    );

    // The same stranger, sending its cookie back from an address of its
    // own, gets key math again, but only as much as one source gets.
    let mut returner = Stranger::at(
        Arc::clone(&stranger),
        Ipv4Addr::new(127, 0, 0, 30),
        host_addr,
        invite.host_key,
    );
    let (knock, mac1) = returner.initiation(None);
    let knocked = Instant::now();
    returner.send(&knock);
    let cookie = returner
        .cookie(&mac1, Duration::from_secs(1))
        .expect("a cookie reply to the stranger");
    let (with_cookie, _) = returner.initiation(Some(&cookie));
    let before = reads();
    let began = Instant::now();
    let mut sent = 0u64;
    while began.elapsed() < Duration::from_secs(2) {
        let due = (began.elapsed().as_secs_f64() * 100.0) as u64;
        while sent < due {
            returner.send(&with_cookie);
            sent += 1;
        }
        thread::sleep(Duration::from_millis(5));
    }
    let spent = knocked.elapsed();
    thread::sleep(Duration::from_millis(200));
    let read = reads() - before;
    // A burst of 20, less the one the cookie reply took, then 10 a second
    // from the knock on. One more for a host a little behind.
    let most = 20 + (10.0 * spent.as_secs_f64()).ceil() as u64;
    assert!(
        read > 20 && read <= most,
        "{read} of {sent} initiations with a cookie read in {spent:?}, at most {most} expected"
    );

    let stopped = flood.stop();
    assert_eq!(
        flood.other_replies(),
        0,
        "the flood got more than cookie replies"
    );
    let hosted = host.wait_for(Duration::from_secs(1), "the numbers", |v| {
        v.numbers.cookie_replies > 0 && v.numbers.dropped_no_cookie > v.numbers.cookie_replies
    });
    println!(
        "flood: {} cookie replies taken, host sent {} and dropped {} for want of mac2",
        flood.cookie_replies(),
        hosted.numbers.cookie_replies,
        hosted.numbers.dropped_no_cookie
    );

    // The count drops under the threshold within a second of the flood
    // stopping, and load lasts 5 s past that.
    let mut probe = Stranger::at(
        stranger,
        Ipv4Addr::new(127, 0, 0, 40),
        host_addr,
        invite.host_key,
    );
    sleep_until(stopped + Duration::from_secs(4));
    let (knock, mac1) = probe.initiation(None);
    probe.send(&knock);
    assert!(
        probe.cookie(&mac1, Duration::from_secs(1)).is_some(),
        "no longer under load 4 s after the flood"
    );
    sleep_until(stopped + Duration::from_millis(7500));
    let before = reads();
    let (knock, mac1) = probe.initiation(None);
    probe.send(&knock);
    poll(
        Duration::from_secs(1),
        "key math for a knock after the load",
        || (reads() > before).then_some(()),
    );
    assert!(
        probe.cookie(&mac1, Duration::from_millis(300)).is_none(),
        "a cookie reply 7.5 s after the flood"
    );

    drop(ana);
    let text = read_log(&log);
    assert!(text.contains("cookie reply taken"), "{text}");
}
