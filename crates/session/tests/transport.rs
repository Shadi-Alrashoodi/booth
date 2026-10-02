mod common;

use std::time::{Duration, Instant};

use common::*;
use session::{
    DATA_OVERHEAD, MAX_PLAINTEXT_LEN, REJECT_AFTER, REJECT_AFTER_MESSAGES, REKEY_AFTER, Received,
    SessionError, Timers, data_receiver_index,
};

#[test]
fn host_never_sends_first() {
    let peers = peers();
    let (mut client, mut host) = handshake(&peers, Instant::now());
    let mut wire = Vec::new();
    let mut plain = Vec::new();

    assert_eq!(
        host.encrypt(b"x", &mut wire),
        Err(SessionError::NotConfirmed)
    );

    // A packet that fails to decrypt must not confirm the session.
    client.encrypt(b"real", &mut wire).expect("encrypt");
    let forged = flipped(&wire, wire.len() - 1, 0x01);
    assert_eq!(
        host.decrypt(&forged, &mut plain),
        Err(SessionError::Decrypt)
    );
    assert!(plain.is_empty());
    assert!(!host.is_confirmed());
    assert_eq!(
        host.encrypt(b"x", &mut wire),
        Err(SessionError::NotConfirmed)
    );

    client.encrypt(b"real", &mut wire).expect("encrypt");
    host.decrypt(&wire, &mut plain).expect("decrypt");
    assert!(host.is_confirmed());
    host.encrypt(b"now I may", &mut wire)
        .expect("host sends after confirmation");
    client.decrypt(&wire, &mut plain).expect("client decrypts");
    assert_eq!(plain, b"now I may");
}

#[test]
fn replays_and_the_newest_flag() {
    let peers = peers();
    let (mut client, mut host) = handshake(&peers, Instant::now());
    let mut packets = Vec::new();
    for n in 0u8..5 {
        let mut wire = Vec::new();
        client.encrypt(&[n], &mut wire).expect("encrypt");
        packets.push(wire);
    }
    let mut plain = Vec::new();
    let mut deliver = |at: usize| host.decrypt(&packets[at], &mut plain);

    let newest = |counter| {
        Ok(Received {
            counter,
            newest: true,
        })
    };
    let late = |counter| {
        Ok(Received {
            counter,
            newest: false,
        })
    };
    assert_eq!(deliver(0), newest(0));
    assert_eq!(deliver(2), newest(2));
    assert_eq!(deliver(1), late(1));
    assert_eq!(deliver(4), newest(4));
    assert_eq!(deliver(3), late(3));
    assert_eq!(deliver(2), Err(SessionError::Replayed));
    assert_eq!(deliver(0), Err(SessionError::Replayed));
    assert_eq!(deliver(4), Err(SessionError::Replayed));
}

#[test]
fn older_than_the_window() {
    let peers = peers();
    let (mut client, mut host) = handshake(&peers, Instant::now());
    let mut first = Vec::new();
    client.encrypt(b"first", &mut first).expect("encrypt");
    let mut wire = Vec::new();
    for _ in 0..9000 {
        client.encrypt(b"filler", &mut wire).expect("encrypt");
    }
    let mut plain = Vec::new();
    host.decrypt(&wire, &mut plain).expect("latest decrypts");
    assert_eq!(
        host.decrypt(&first, &mut plain),
        Err(SessionError::Replayed)
    );
}

#[test]
fn flipped_data_bytes() {
    let peers = peers();
    let (mut client, mut host) = handshake(&peers, Instant::now());
    let mut wire = Vec::new();
    client.encrypt(b"some payload", &mut wire).expect("encrypt");
    let mut plain = Vec::new();
    for at in 0..wire.len() {
        for bits in [0x01, 0x80, 0xff] {
            let changed = flipped(&wire, at, bits);
            assert!(
                host.decrypt(&changed, &mut plain).is_err(),
                "byte {at} xor {bits:#x} was accepted"
            );
            assert!(plain.is_empty());
        }
    }
    // None of the failures touched the replay window.
    assert_eq!(
        host.decrypt(&wire, &mut plain),
        Ok(Received {
            counter: 0,
            newest: true
        })
    );
    assert_eq!(plain, b"some payload");
}

#[test]
fn header_checks() {
    let peers = peers();
    let (mut client, mut host) = handshake(&peers, Instant::now());
    let mut wire = Vec::new();
    let mut plain = Vec::new();
    client
        .encrypt(b"", &mut wire)
        .expect("empty payload is fine");
    assert_eq!(wire.len(), DATA_OVERHEAD);
    assert_eq!(data_receiver_index(&wire), Some(host.local_index()));

    assert_eq!(
        host.decrypt(&wire[..DATA_OVERHEAD - 1], &mut plain),
        Err(SessionError::Malformed)
    );
    assert_eq!(data_receiver_index(&wire[..DATA_OVERHEAD - 1]), None);

    let mut reserved = wire.clone();
    reserved[3] = 1;
    assert_eq!(
        host.decrypt(&reserved, &mut plain),
        Err(SessionError::Malformed)
    );
    assert_eq!(data_receiver_index(&reserved), None);

    let mut other_index = wire.clone();
    other_index[4..8].copy_from_slice(&(host.local_index() ^ 1).to_le_bytes());
    assert_eq!(
        host.decrypt(&other_index, &mut plain),
        Err(SessionError::WrongIndex)
    );

    // Past the limit is refused before any decryption is tried.
    for counter in [REJECT_AFTER_MESSAGES, u64::MAX] {
        let mut limit = wire.clone();
        limit[8..16].copy_from_slice(&counter.to_le_bytes());
        assert_eq!(
            host.decrypt(&limit, &mut plain),
            Err(SessionError::CounterLimit)
        );
    }

    host.decrypt(&wire, &mut plain).expect("original decrypts");
    assert!(plain.is_empty());
}

#[test]
fn payload_size_limit() {
    let peers = peers();
    let (mut client, mut host) = handshake(&peers, Instant::now());
    let mut wire = Vec::new();
    let mut plain = Vec::new();
    let biggest = vec![0xab; MAX_PLAINTEXT_LEN];
    client
        .encrypt(&biggest, &mut wire)
        .expect("largest payload");
    assert_eq!(wire.len(), 65_507);
    host.decrypt(&wire, &mut plain).expect("decrypts");
    assert_eq!(plain, biggest);

    assert_eq!(
        client.encrypt(&vec![0; MAX_PLAINTEXT_LEN + 1], &mut wire),
        Err(SessionError::TooLarge)
    );
    assert!(wire.is_empty());
}

#[test]
fn oversized_packet_is_refused_before_allocating() {
    let peers = peers();
    let (mut client, mut host) = handshake(&peers, Instant::now());
    let mut wire = Vec::new();
    client.encrypt(b"x", &mut wire).expect("encrypt");
    // Right index, fresh counter, a megabyte long.
    wire.resize(1 << 20, 0);
    assert_eq!(data_receiver_index(&wire), None);
    let mut plain = Vec::new();
    assert_eq!(
        host.decrypt(&wire, &mut plain),
        Err(SessionError::Malformed)
    );
    assert_eq!(plain.capacity(), 0);

    wire.truncate(MAX_PLAINTEXT_LEN + DATA_OVERHEAD + 1);
    assert_eq!(
        host.decrypt(&wire, &mut plain),
        Err(SessionError::Malformed)
    );
}

#[test]
fn sessions_do_not_read_each_other() {
    let peers = peers();
    let now = Instant::now();
    let (mut first_client, _first_host) = handshake(&peers, now);
    let (_second_client, mut second_host) = handshake(&peers, now);
    let mut wire = Vec::new();
    let mut plain = Vec::new();
    first_client.encrypt(b"x", &mut wire).expect("encrypt");
    // Same indices on purpose: only the keys differ.
    assert_eq!(
        second_host.decrypt(&wire, &mut plain),
        Err(SessionError::Decrypt)
    );
}

#[test]
fn rekey_and_expiry_follow_the_clock() {
    let peers = peers();
    let start = Instant::now();
    let (client, host) = handshake(&peers, start);
    let at = |seconds: u64| start + Duration::from_secs(seconds);

    assert!(!client.needs_rekey(start));
    assert!(!client.needs_rekey(at(119)));
    assert!(client.needs_rekey(start + REKEY_AFTER));
    assert!(client.needs_rekey(at(150)));
    assert!(
        !host.needs_rekey(at(150)),
        "only the initiator starts a rekey"
    );

    for session in [&client, &host] {
        assert!(!session.is_expired(start));
        assert!(!session.is_expired(at(179)));
        assert!(session.is_expired(start + REJECT_AFTER));
        assert!(session.is_expired(at(10_000)));
    }

    // A clock reading from before the session began counts as age zero.
    let (late_client, _) = handshake(&peers, at(60));
    assert!(!late_client.needs_rekey(start));
    assert!(!late_client.is_expired(start));
}

// Prints numbers, asserts nothing. Run with --nocapture, and with --release for real figures.
#[test]
fn data_packet_time() {
    let peers = peers();
    let (mut client, mut host) = handshake(&peers, Instant::now());
    let payload = vec![0x5a; 1200 - DATA_OVERHEAD];
    let mut wire = Vec::with_capacity(1500);
    let mut plain = Vec::with_capacity(1500);
    let rounds: u32 = if cfg!(debug_assertions) {
        1_000
    } else {
        20_000
    };
    let mut sealing = Duration::ZERO;
    let mut opening = Duration::ZERO;
    for _ in 0..rounds {
        let t0 = Instant::now();
        client.encrypt(&payload, &mut wire).expect("encrypt");
        let t1 = Instant::now();
        host.decrypt(&wire, &mut plain).expect("decrypt");
        let t2 = Instant::now();
        sealing += t1 - t0;
        opening += t2 - t1;
    }
    let build = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    println!(
        "1200 byte data packet ({build} build, {rounds} rounds): encrypt {:.2} us, decrypt {:.2} us",
        sealing.as_secs_f64() * 1e6 / f64::from(rounds),
        opening.as_secs_f64() * 1e6 / f64::from(rounds),
    );
}

#[test]
fn timers_can_be_shortened() {
    let peers = peers();
    let start = Instant::now();
    let (client, host) = handshake(&peers, start);
    let timers = Timers {
        rekey_after: Duration::from_millis(200),
        reject_after: Duration::from_millis(300),
    };
    let client = client.with_timers(timers);
    let host = host.with_timers(timers);
    assert!(!client.needs_rekey(start + Duration::from_millis(199)));
    assert!(client.needs_rekey(start + Duration::from_millis(200)));
    assert!(!host.is_expired(start + Duration::from_millis(299)));
    assert!(host.is_expired(start + Duration::from_millis(300)));
    assert_eq!(Timers::default().rekey_after, REKEY_AFTER);
    assert_eq!(Timers::default().reject_after, REJECT_AFTER);
}

#[test]
fn timers_cannot_be_lengthened() {
    let peers = peers();
    let start = Instant::now();
    let (client, host) = handshake(&peers, start);
    let forever = Timers {
        rekey_after: Duration::MAX,
        reject_after: Duration::MAX,
    };
    let client = client.with_timers(forever);
    let host = host.with_timers(forever);
    assert!(client.needs_rekey(start + REKEY_AFTER));
    assert!(client.is_expired(start + REJECT_AFTER));
    assert!(host.is_expired(start + REJECT_AFTER));

    // A rekey that would come after the session is dropped comes when it is dropped instead.
    let (client, _) = handshake(&peers, start);
    let client = client.with_timers(Timers {
        rekey_after: Duration::from_millis(500),
        reject_after: Duration::from_millis(300),
    });
    assert!(!client.needs_rekey(start + Duration::from_millis(299)));
    assert!(client.needs_rekey(start + Duration::from_millis(300)));
}

// A capture thread seals voice while the room thread encrypts control and
// pings on the same session. Every packet gets a counter of its own, so the
// far side takes each one exactly once.
#[test]
fn sealer_and_session_counters_never_collide() {
    let peers = peers();
    let (mut client, mut host) = handshake(&peers, Instant::now());
    let sealer = client.sealer().expect("the initiator may send at once");
    assert_eq!(sealer.remote_index(), host.local_index());
    const EACH: usize = 3000;
    let sealing = std::thread::spawn(move || {
        let mut out = Vec::with_capacity(128);
        let mut packets = Vec::with_capacity(EACH);
        for _ in 0..EACH {
            sealer.seal(b"voice", &mut out).expect("seal");
            packets.push(out.clone());
        }
        packets
    });
    let mut packets = Vec::with_capacity(2 * EACH);
    let mut wire = Vec::new();
    for _ in 0..EACH {
        client.encrypt(b"control", &mut wire).expect("encrypt");
        packets.push(wire.clone());
    }
    packets.extend(sealing.join().expect("the sealing thread"));

    let mut counters: Vec<u64> = packets
        .iter()
        .map(|packet| u64::from_le_bytes(packet[8..16].try_into().unwrap()))
        .collect();
    counters.sort_unstable();
    counters.dedup();
    assert_eq!(counters.len(), 2 * EACH);
    // In counter order the window never moves past one, so every packet is
    // taken; a copy of any is then a replay.
    packets.sort_by_key(|packet| u64::from_le_bytes(packet[8..16].try_into().unwrap()));
    let mut plain = Vec::new();
    for packet in &packets {
        host.decrypt(packet, &mut plain).expect("decrypts once");
        assert!(plain == b"voice" || plain == b"control");
    }
    assert_eq!(
        host.decrypt(&packets[17], &mut plain),
        Err(SessionError::Replayed)
    );
}

// WireGuard's rule holds for the sealer too: the host has none until the
// client has sent on the session.
#[test]
fn host_sealer_waits_for_confirmation() {
    let peers = peers();
    let (mut client, mut host) = handshake(&peers, Instant::now());
    assert!(host.sealer().is_none());
    let mut wire = Vec::new();
    let mut plain = Vec::new();
    client.encrypt(b"hello", &mut wire).expect("encrypt");
    host.decrypt(&wire, &mut plain).expect("decrypt");
    let sealer = host.sealer().expect("confirmed now");
    sealer.seal(b"voice", &mut wire).expect("seal");
    client
        .decrypt(&wire, &mut plain)
        .expect("the client opens it");
    assert_eq!(plain, b"voice");
    assert_eq!(wire.len(), DATA_OVERHEAD + 5);
    assert_eq!(
        sealer.seal(&vec![0; MAX_PLAINTEXT_LEN + 1], &mut wire),
        Err(SessionError::TooLarge)
    );
}
