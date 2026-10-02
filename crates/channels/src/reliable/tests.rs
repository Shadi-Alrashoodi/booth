use std::collections::VecDeque;
use std::time::{Duration, Instant};

use proptest::prelude::*;

use super::wire::{self, Ack, Frame};
use super::*;

mod simulator;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn transmit_all(r: &mut Reliable, now: Instant, timeout: Option<Duration>) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    while let Some(frame) = r.poll_transmit(now, timeout) {
        frames.push(frame);
    }
    frames
}

fn take_delivered(r: &mut Reliable) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(message) = r.next_delivered() {
        out.push(message);
    }
    out
}

// Everything from one side reaches the other at the same instant, then the
// answers come back.
fn exchange(from: &mut Reliable, to: &mut Reliable, now: Instant, timeout: Option<Duration>) {
    for frame in transmit_all(from, now, timeout) {
        to.receive(&frame, now).unwrap();
    }
    for frame in transmit_all(to, now, timeout) {
        from.receive(&frame, now).unwrap();
    }
}

fn send_what_fits(r: &mut Reliable, to_send: &mut VecDeque<Vec<u8>>) {
    while let Some(message) = to_send.front() {
        if r.send(message).is_err() {
            break;
        }
        to_send.pop_front();
    }
}

fn data_seq(frame: &[u8]) -> u32 {
    match wire::parse(frame) {
        Ok(Frame::Data { seq, .. }) => seq,
        other => panic!("expected a data frame, got {other:?}"),
    }
}

fn numbered(count: usize) -> Vec<Vec<u8>> {
    (0..count)
        .map(|i| format!("message {i}").into_bytes())
        .collect()
}

#[test]
fn delivers_in_order_over_a_clean_link() {
    let now = Instant::now();
    let mut a = Reliable::new();
    let mut b = Reliable::new();
    let messages = numbered(10);
    for m in &messages {
        a.send(m).unwrap();
    }
    for frame in transmit_all(&mut a, now, None) {
        b.receive(&frame, now).unwrap();
    }
    assert_eq!(take_delivered(&mut b), messages);

    let acks = transmit_all(&mut b, now, None);
    assert_eq!(acks.len(), 1, "one ack covers everything received");
    a.receive(&acks[0], now).unwrap();
    assert_eq!(a.counters().in_flight, 0);
    assert_eq!(a.counters().sent, 10);
    assert_eq!(a.next_timeout(), None);
}

#[test]
fn holds_out_of_order_messages_until_the_gap_fills() {
    let now = Instant::now();
    let mut a = Reliable::new();
    let mut b = Reliable::new();
    let messages = numbered(4);
    for m in &messages {
        a.send(m).unwrap();
    }
    let frames = transmit_all(&mut a, now, None);

    for i in [3, 1, 2] {
        b.receive(&frames[i], now).unwrap();
    }
    assert_eq!(b.next_delivered(), None);
    b.receive(&frames[0], now).unwrap();
    assert_eq!(take_delivered(&mut b), messages);
}

#[test]
fn duplicate_is_dropped_and_acked_again() {
    let now = Instant::now();
    let mut a = Reliable::new();
    let mut b = Reliable::new();
    a.send(b"once").unwrap();
    let frame = a.poll_transmit(now, None).unwrap();

    b.receive(&frame, now).unwrap();
    assert_eq!(transmit_all(&mut b, now, None).len(), 1);
    b.receive(&frame, now).unwrap();
    let again = transmit_all(&mut b, now, None);
    assert_eq!(again.len(), 1, "a duplicate must be acked again");
    assert_eq!(
        wire::parse(&again[0]).unwrap(),
        Frame::Ack(Ack { next: 1, bits: 0 })
    );
    assert_eq!(take_delivered(&mut b), vec![b"once".to_vec()]);
}

#[test]
fn ack_bits_report_held_messages() {
    let now = Instant::now();
    let mut a = Reliable::new();
    let mut b = Reliable::new();
    for m in numbered(6) {
        a.send(&m).unwrap();
    }
    let frames = transmit_all(&mut a, now, None);
    for i in [0, 2, 5] {
        b.receive(&frames[i], now).unwrap();
    }
    let ack = transmit_all(&mut b, now, None);
    assert_eq!(ack.len(), 1);
    // next = 1; seq 2 is bit 0, seq 5 is bit 3.
    let expected: [u8; 13] = [1, 1, 0, 0, 0, 0b1001, 0, 0, 0, 0, 0, 0, 0];
    assert_eq!(ack[0], expected);

    a.receive(&ack[0], now).unwrap();
    let counters = a.counters();
    assert_eq!(counters.in_flight, 3, "seqs 1, 3 and 4 are still missing");
}

#[test]
fn data_frames_carry_the_current_ack() {
    let now = Instant::now();
    let mut a = Reliable::new();
    let mut b = Reliable::new();
    a.send(b"ping").unwrap();
    b.receive(&a.poll_transmit(now, None).unwrap(), now)
        .unwrap();
    b.send(b"reply").unwrap();

    let frames = transmit_all(&mut b, now, None);
    assert_eq!(
        frames.len(),
        1,
        "the reply carries the ack, no separate ack"
    );
    match wire::parse(&frames[0]).unwrap() {
        Frame::Data { ack, seq, message } => {
            assert_eq!(ack, Ack { next: 1, bits: 0 });
            assert_eq!(seq, 0);
            assert_eq!(message, b"reply");
        }
        other => panic!("expected data, got {other:?}"),
    }
}

#[test]
fn sends_at_most_a_window_before_acks() {
    let now = Instant::now();
    let mut a = Reliable::new();
    for m in numbered(100) {
        a.send(&m).unwrap();
    }
    let first = transmit_all(&mut a, now, None);
    assert_eq!(first.len(), WINDOW);
    assert_eq!(a.counters().in_flight, WINDOW);
    assert_eq!(a.counters().queued, 100 - WINDOW);

    let ack = wire::ack_only(Ack { next: 10, bits: 0 });
    a.receive(&ack, now).unwrap();
    let more = transmit_all(&mut a, now, None);
    let seqs: Vec<u32> = more.iter().map(|f| data_seq(f)).collect();
    assert_eq!(seqs, (64..74).collect::<Vec<u32>>());
}

#[test]
fn selective_ack_does_not_open_the_window() {
    let now = Instant::now();
    let mut a = Reliable::new();
    for m in numbered(100) {
        a.send(&m).unwrap();
    }
    transmit_all(&mut a, now, None);
    // Seq 64 was never sent, so the top bit is a bad ack.
    let too_far = wire::ack_only(Ack {
        next: 0,
        bits: u64::MAX,
    });
    assert_eq!(
        a.receive(&too_far, now),
        Err(ReliableError::BadAck {
            acked: 64,
            next_to_send: 64
        })
    );
    // Everything but seq 0 received: the receiver still has to hold 1..=63,
    // so the sender must not run past 0 + WINDOW.
    let ack = wire::ack_only(Ack {
        next: 0,
        bits: u64::MAX >> 1,
    });
    a.receive(&ack, now).unwrap();
    assert_eq!(a.counters().in_flight, 1);
    assert!(transmit_all(&mut a, now, None).is_empty());
}

#[test]
fn a_lost_head_costs_one_retransmission_not_a_window() {
    let start = Instant::now();
    let timeout = Some(ms(25));
    let mut a = Reliable::new();
    let mut b = Reliable::new();
    for m in numbered(WINDOW) {
        a.send(&m).unwrap();
    }
    let frames = transmit_all(&mut a, start, timeout);
    for frame in &frames[1..] {
        b.receive(frame, start).unwrap();
    }
    for frame in transmit_all(&mut b, start, timeout) {
        a.receive(&frame, start).unwrap();
    }
    assert_eq!(a.counters().in_flight, 1, "b reported all 63 it holds");

    // The head keeps getting lost. Only it is ever sent again.
    for _ in 0..3 {
        let now = a.next_timeout().unwrap();
        let seqs: Vec<u32> = transmit_all(&mut a, now, timeout)
            .iter()
            .map(|f| data_seq(f))
            .collect();
        assert_eq!(seqs, vec![0]);
    }
    assert_eq!(a.counters().retransmissions, 3);
}

// Karn's rule: once a message went out twice, its ack could answer either
// copy, so only messages acked on their first send give a delay.
#[test]
fn ack_delays_follow_karns_rule() {
    let start = Instant::now();
    let timeout = Some(ms(50));
    let mut a = Reliable::new();
    let mut b = Reliable::new();
    for m in numbered(3) {
        a.send(&m).unwrap();
    }
    let frames = transmit_all(&mut a, start, timeout);
    // The first is lost and sent again; the other two arrive.
    for frame in &frames[1..] {
        b.receive(frame, start + ms(10)).unwrap();
    }
    for frame in transmit_all(&mut b, start + ms(10), timeout) {
        a.receive(&frame, start + ms(30)).unwrap();
    }
    assert_eq!(a.ack_delays().collect::<Vec<_>>(), [ms(30), ms(30)]);
    assert_eq!(a.ack_delays().count(), 0, "taken once");

    let resent = transmit_all(&mut a, start + ms(50), timeout);
    assert_eq!(resent.iter().map(|f| data_seq(f)).collect::<Vec<_>>(), [0]);
    b.receive(&resent[0], start + ms(60)).unwrap();
    for frame in transmit_all(&mut b, start + ms(60), timeout) {
        a.receive(&frame, start + ms(70)).unwrap();
    }
    assert_eq!(a.counters().in_flight, 0);
    assert_eq!(a.ack_delays().count(), 0, "a resent message gave a delay");

    // A path that came back resets the backoff, not what went out twice.
    a.send(b"late").unwrap();
    let late = transmit_all(&mut a, start + ms(100), timeout);
    let again = transmit_all(&mut a, start + ms(150), timeout);
    assert_eq!(again.len(), 1);
    a.path_recovered(start + ms(160));
    b.receive(&late[0], start + ms(170)).unwrap();
    for frame in transmit_all(&mut b, start + ms(170), timeout) {
        a.receive(&frame, start + ms(180)).unwrap();
    }
    assert_eq!(a.ack_delays().count(), 0);
}

#[test]
fn retransmit_timeout_is_clamped() {
    let start = Instant::now();
    let cases = [
        (None, ms(200)),
        (Some(ms(25)), ms(25)),
        (Some(ms(1)), ms(20)),
        (Some(Duration::ZERO), ms(20)),
        (Some(ms(3000)), ms(1000)),
    ];
    for (timeout, waits) in cases {
        let mut a = Reliable::new();
        a.send(b"x").unwrap();
        assert_eq!(transmit_all(&mut a, start, timeout).len(), 1);
        assert_eq!(a.next_timeout(), Some(start + waits), "timeout {timeout:?}");
    }
}

#[test]
fn retransmit_timeout_doubles_up_to_two_seconds() {
    let start = Instant::now();
    let timeout = Some(ms(25));
    let mut a = Reliable::new();
    a.send(b"lost every time").unwrap();
    let first = a.poll_transmit(start, timeout).unwrap();

    let mut now = start;
    let mut waits = Vec::new();
    for _ in 0..9 {
        let deadline = a.next_timeout().unwrap();
        assert_eq!(
            a.poll_transmit(deadline - Duration::from_micros(1), timeout),
            None
        );
        waits.push(deadline - now);
        now = deadline;
        let again = a.poll_transmit(now, timeout).unwrap();
        assert_eq!(again, first);
    }
    let expected: Vec<Duration> = [25, 50, 100, 200, 400, 800, 1600, 2000, 2000]
        .into_iter()
        .map(ms)
        .collect();
    assert_eq!(waits, expected);
    assert_eq!(a.counters().retransmissions, 9);
    assert_eq!(a.counters().sent, 1);
}

// One message sent at start, every copy lost until 1580 ms. Its copies went
// out at 25, 75, 175, 375, 775 and 1575 ms, so the next one is due at 3175.
fn backed_off_by_an_outage(start: Instant, timeout: Option<Duration>) -> Reliable {
    let mut a = Reliable::new();
    a.send(b"sent into the outage").unwrap();
    a.poll_transmit(start, timeout).unwrap();
    while let Some(deadline) = a.next_timeout()
        && deadline <= start + ms(1580)
    {
        a.poll_transmit(deadline, timeout).unwrap();
    }
    assert_eq!(a.next_timeout(), Some(start + ms(3175)));
    a
}

#[test]
fn new_data_from_the_peer_ends_the_backoff() {
    let start = Instant::now();
    let timeout = Some(ms(25));
    let mut a = backed_off_by_an_outage(start, timeout);

    let back = start + ms(1580);
    let mut b = Reliable::new();
    b.send(b"the peer talks again").unwrap();
    a.receive(&b.poll_transmit(back, timeout).unwrap(), back)
        .unwrap();
    assert_eq!(a.next_timeout(), Some(back + ms(25)));

    // The backoff starts over from the normal timeout.
    let resend = back + ms(25);
    assert_eq!(data_seq(&a.poll_transmit(resend, timeout).unwrap()), 0);
    assert_eq!(a.next_timeout(), Some(resend + ms(50)));
}

#[test]
fn path_recovered_ends_the_backoff() {
    let start = Instant::now();
    let timeout = Some(ms(25));
    let mut a = backed_off_by_an_outage(start, timeout);
    let back = start + ms(1580);
    a.path_recovered(back);
    assert_eq!(a.next_timeout(), Some(back + ms(25)));
    // A second call before anything is resent changes nothing.
    a.path_recovered(back + ms(10));
    assert_eq!(a.next_timeout(), Some(back + ms(25)));
}

#[test]
fn a_repeated_ack_keeps_the_backoff() {
    let start = Instant::now();
    let timeout = Some(ms(25));
    let mut a = backed_off_by_an_outage(start, timeout);
    let ack = wire::ack_only(Ack { next: 0, bits: 0 });
    a.receive(&ack, start + ms(1580)).unwrap();
    assert_eq!(a.next_timeout(), Some(start + ms(3175)));
}

#[test]
fn delivers_within_a_round_trip_once_an_outage_ends() {
    let timeout = Some(ms(25));
    let one_way = ms(5);
    for outage in [300, 800, 1580, 3200, 5200].map(ms) {
        let start = Instant::now();
        let back = start + outage;
        let mut a = Reliable::new();
        let mut b = Reliable::new();
        a.send(b"sent into the outage").unwrap();
        let mut a_to_b: Vec<(Instant, Vec<u8>)> = Vec::new();
        let mut b_to_a: Vec<(Instant, Vec<u8>)> = Vec::new();

        let mut now = start;
        let delivered_at = loop {
            if now == back {
                b.send(b"the peer talks again").unwrap();
            }
            for (to, link) in [(&mut b, &mut a_to_b), (&mut a, &mut b_to_a)] {
                link.retain(|(at, frame)| {
                    if *at > now {
                        return true;
                    }
                    to.receive(frame, now).unwrap();
                    false
                });
            }
            if b.next_delivered().is_some() {
                break now;
            }
            for (from, link) in [(&mut a, &mut a_to_b), (&mut b, &mut b_to_a)] {
                for frame in transmit_all(from, now, timeout) {
                    if now >= back {
                        link.push((now + one_way, frame));
                    }
                }
            }
            now += ms(1);
            assert!(now < back + ms(3000), "outage {outage:?}: never delivered");
        };
        // The peer's frame arrives 5 ms after the path is back, the resend
        // goes out one 25 ms timeout later and takes 5 ms.
        let after = delivered_at - back;
        assert!(
            after <= ms(35),
            "outage {outage:?}: delivered {after:?} after the path came back"
        );
    }
}

#[test]
fn retransmits_oldest_expired_first_then_new_messages() {
    let start = Instant::now();
    let mut a = Reliable::new();
    a.send(b"zero").unwrap();
    a.send(b"one").unwrap();
    transmit_all(&mut a, start, None);
    a.send(b"two").unwrap();

    let later = start + ms(200);
    let frames = transmit_all(&mut a, later, None);
    let seqs: Vec<u32> = frames.iter().map(|f| data_seq(f)).collect();
    assert_eq!(seqs, vec![0, 1, 2]);
}

#[test]
fn rejects_oversize_messages_and_a_full_queue() {
    let mut a = Reliable::new();
    assert_eq!(
        a.send(&[0; MAX_MESSAGE + 1]),
        Err(ReliableError::TooBig(MAX_MESSAGE + 1))
    );
    a.send(&[0; MAX_MESSAGE]).unwrap();
    for _ in 1..MAX_QUEUED {
        a.send(b"").unwrap();
    }
    assert_eq!(a.send(b""), Err(ReliableError::Full));

    // Sending moves messages into flight but they still count.
    transmit_all(&mut a, Instant::now(), None);
    assert_eq!(a.send(b""), Err(ReliableError::Full));
}

// Everything but what the peer confirmed, cumulatively or selectively, in
// the order it was sent, including what never left for want of window.
#[test]
fn unacked_is_what_the_peer_has_not_confirmed_in_order() {
    let now = Instant::now();
    let mut a = Reliable::new();
    let mut b = Reliable::new();
    assert_eq!(a.unacked().count(), 0);
    let messages = numbered(WINDOW + 6);
    for m in &messages {
        a.send(m).unwrap();
    }
    let frames = transmit_all(&mut a, now, None);
    assert_eq!(frames.len(), WINDOW);
    for i in [0, 2] {
        b.receive(&frames[i], now).unwrap();
    }
    for ack in transmit_all(&mut b, now, None) {
        a.receive(&ack, now).unwrap();
    }
    let want: Vec<&[u8]> = messages
        .iter()
        .enumerate()
        .filter(|(i, _)| ![0, 2].contains(i))
        .map(|(_, m)| m.as_slice())
        .collect();
    assert_eq!(a.unacked().collect::<Vec<_>>(), want);
}

#[test]
fn largest_message_fits_a_frame() {
    let mut a = Reliable::new();
    a.send(&[7; MAX_MESSAGE]).unwrap();
    let frame = a.poll_transmit(Instant::now(), None).unwrap();
    assert_eq!(frame.len(), MAX_FRAME);
    let mut b = Reliable::new();
    b.receive(&frame, Instant::now()).unwrap();
    assert_eq!(b.next_delivered(), Some(vec![7; MAX_MESSAGE]));
}

#[test]
fn rejects_malformed_frames() {
    let now = Instant::now();
    let mut data = wire::data(Ack { next: 0, bits: 0 }, 0, &[1; MAX_MESSAGE]);
    data.push(1);
    let mut unknown = vec![2];
    unknown.extend_from_slice(&[0; 12]);
    let cases: Vec<(Vec<u8>, ReliableError)> = vec![
        (vec![], ReliableError::Length(0)),
        (unknown, ReliableError::UnknownKind(2)),
        (vec![255], ReliableError::UnknownKind(255)),
        (vec![1; 12], ReliableError::Length(12)),
        (vec![1; 14], ReliableError::Length(14)),
        (vec![0; 16], ReliableError::Length(16)),
        (data, ReliableError::TooBig(MAX_MESSAGE + 1)),
    ];
    for (frame, error) in cases {
        let mut r = Reliable::new();
        assert_eq!(r.receive(&frame, now), Err(error), "{frame:?}");
        assert_eq!(r.poll_transmit(now, None), None, "a bad frame owes no ack");
    }
}

#[test]
fn rejects_acks_for_messages_never_sent() {
    let now = Instant::now();
    let mut a = Reliable::new();
    a.send(b"only one").unwrap();
    transmit_all(&mut a, now, None);

    for (ack, acked) in [
        (Ack { next: 2, bits: 0 }, 1),
        (
            Ack {
                next: 0,
                bits: 0b10,
            },
            2,
        ),
    ] {
        let frame = wire::ack_only(ack);
        assert_eq!(
            a.receive(&frame, now),
            Err(ReliableError::BadAck {
                acked,
                next_to_send: 1
            })
        );
    }
    // A data frame with a bad ack is dropped whole: its message is not
    // delivered.
    let frame = wire::data(Ack { next: 5, bits: 0 }, 0, b"smuggled");
    assert_eq!(
        a.receive(&frame, now),
        Err(ReliableError::BadAck {
            acked: 4,
            next_to_send: 1
        })
    );
    assert_eq!(a.next_delivered(), None);
    assert_eq!(a.counters().in_flight, 1);
}

#[test]
fn a_one_sided_reset_names_both_sequences() {
    let now = Instant::now();
    let mut host = Reliable::new();
    let mut client = Reliable::new();
    for m in numbered(300) {
        client.send(&m).unwrap();
    }
    for _ in 0..300 / WINDOW + 1 {
        exchange(&mut client, &mut host, now, None);
    }
    assert_eq!(take_delivered(&mut host).len(), 300);
    assert_eq!(client.counters().in_flight, 0);

    // The client restarted and the host kept its state.
    let mut client = Reliable::new();
    host.send(b"hello again").unwrap();
    let frame = host.poll_transmit(now, None).unwrap();
    let error = client.receive(&frame, now).unwrap_err();
    assert_eq!(
        error,
        ReliableError::BadAck {
            acked: 299,
            next_to_send: 0
        }
    );
    assert_eq!(
        error.to_string(),
        "ack covers sequence 299 but the next one to send is 0"
    );
}

#[test]
fn stale_ack_is_harmless() {
    let now = Instant::now();
    let mut a = Reliable::new();
    for m in numbered(5) {
        a.send(&m).unwrap();
    }
    transmit_all(&mut a, now, None);
    a.receive(&wire::ack_only(Ack { next: 3, bits: 0 }), now)
        .unwrap();
    a.receive(
        &wire::ack_only(Ack {
            next: 1,
            bits: 0b10,
        }),
        now,
    )
    .unwrap();
    assert_eq!(a.counters().in_flight, 1);
}

#[test]
fn rejects_data_beyond_the_window() {
    let now = Instant::now();
    let mut b = Reliable::new();
    let ack = Ack { next: 0, bits: 0 };
    let frame = wire::data(ack, WINDOW as u32, b"too far");
    assert_eq!(
        b.receive(&frame, now),
        Err(ReliableError::OutOfWindow(WINDOW as u32))
    );
    let frame = wire::data(ack, WINDOW as u32 - 1, b"just inside");
    b.receive(&frame, now).unwrap();
}

#[test]
fn stops_taking_messages_while_nobody_reads() {
    let mut now = Instant::now();
    let timeout = Some(ms(20));
    let mut a = Reliable::new();
    let mut b = Reliable::new();
    let total = MAX_QUEUED + 200;
    let mut to_send: VecDeque<Vec<u8>> = numbered(total).into();
    for _ in 0..100 {
        send_what_fits(&mut a, &mut to_send);
        exchange(&mut a, &mut b, now, timeout);
    }
    assert_eq!(b.delivered.len(), MAX_QUEUED);
    // b holds a full window it cannot deliver yet, and has told a about all
    // of it but the first, which it does not ack until there is room.
    assert_eq!(b.held.iter().flatten().count(), WINDOW);
    assert_eq!(a.counters().in_flight, 1);

    let mut got = take_delivered(&mut b);
    assert_eq!(got.len(), MAX_QUEUED + WINDOW);
    for _ in 0..100 {
        now += ms(50);
        send_what_fits(&mut a, &mut to_send);
        exchange(&mut a, &mut b, now, timeout);
        got.extend(take_delivered(&mut b));
    }
    assert_eq!(got, numbered(total));
}

#[test]
fn reading_again_reopens_the_window() {
    let mut now = Instant::now();
    let timeout = Some(ms(25));
    let mut a = Reliable::new();
    let mut b = Reliable::new();
    let total = MAX_QUEUED + 300;
    let mut to_send: VecDeque<Vec<u8>> = numbered(total).into();
    // Nobody reads on b for 10 s, long enough for a's retransmissions of the
    // message b cannot take to back off to the cap.
    for _ in 0..200 {
        send_what_fits(&mut a, &mut to_send);
        exchange(&mut a, &mut b, now, timeout);
        now += ms(50);
    }
    now = a.next_timeout().unwrap();
    exchange(&mut a, &mut b, now, timeout);
    assert_eq!(a.next_timeout(), Some(now + ms(2000)));

    // Reading one message frees room for the one that was waiting and the
    // run held behind it. b's ack goes out as soon as it is polled, and a's
    // next window, sent straight back, must land inside b's.
    now += ms(1);
    let mut got = vec![b.next_delivered().unwrap()];
    assert_eq!(b.delivered.len(), MAX_QUEUED - 1 + WINDOW);
    exchange(&mut b, &mut a, now, timeout);
    got.extend(take_delivered(&mut b));
    assert_eq!(got.len(), MAX_QUEUED + 2 * WINDOW);
    assert_eq!(got, numbered(total)[..got.len()]);
}

#[test]
fn ack_bits_cross_the_wrap() {
    let now = Instant::now();
    let start = u32::MAX - 1;
    let mut a = Reliable::starting_at(start);
    let mut b = Reliable::starting_at(start);
    for m in numbered(5) {
        a.send(&m).unwrap();
    }
    let frames = transmit_all(&mut a, now, None);
    let seqs: Vec<u32> = frames.iter().map(|f| data_seq(f)).collect();
    assert_eq!(seqs, vec![u32::MAX - 1, u32::MAX, 0, 1, 2]);

    for i in [0, 2, 4] {
        b.receive(&frames[i], now).unwrap();
    }
    let ack = transmit_all(&mut b, now, None);
    assert_eq!(
        wire::parse(&ack[0]).unwrap(),
        Frame::Ack(Ack {
            next: u32::MAX,
            bits: 0b101,
        })
    );
    a.receive(&ack[0], now).unwrap();
    assert_eq!(a.counters().in_flight, 2);
    b.receive(&frames[1], now).unwrap();
    b.receive(&frames[3], now).unwrap();
    assert_eq!(take_delivered(&mut b), numbered(5));
}

proptest! {
    #[test]
    fn crafted_frames_never_break_state(
        frames in proptest::collection::vec(
            (
                0u8..3,
                prop_oneof![0u32..80, any::<u32>()],
                any::<u64>(),
                prop_oneof![0u32..80, any::<u32>()],
                proptest::collection::vec(any::<u8>(), 0..40),
            ),
            1..40,
        ),
    ) {
        let now = Instant::now();
        let mut r = Reliable::new();
        for m in numbered(80) {
            r.send(&m).unwrap();
        }
        transmit_all(&mut r, now, None);
        for (kind, next, bits, seq, message) in frames {
            let mut frame = vec![kind];
            frame.extend_from_slice(&next.to_le_bytes());
            frame.extend_from_slice(&bits.to_le_bytes());
            if kind != 1 {
                frame.extend_from_slice(&seq.to_le_bytes());
                frame.extend_from_slice(&message);
            }
            let _ = r.receive(&frame, now);
            prop_assert!(r.in_flight.len() <= WINDOW);
            prop_assert!(r.delivered.len() < MAX_QUEUED + WINDOW);
            transmit_all(&mut r, now + ms(250), None);
            while r.next_delivered().is_some() {}
        }
    }
}
