use std::time::{Duration, Instant};

use channels::video::{
    Arrival, DropReason, Event, FRAME_HEADER, FrameFacts, MAX_HELD_BYTES, MAX_PENDING, MAX_SHARD,
    MIN_SHARD, Packet, PacketError, Packetizer, RECOVER_GAP, Reassembler, parity_percent,
    read_packet,
};

const PAYLOAD: usize = 1200 - 32 - 1 - 2;
const SHARD: usize = 1152;
const FPS_120: Duration = Duration::from_nanos(8_333_333);
const FPS_60: Duration = Duration::from_nanos(16_666_667);
const WAIT_120: Duration = Duration::from_millis(10);
const MS: Duration = Duration::from_millis(1);
const US: Duration = Duration::from_micros(1);

struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            items.swap(i, self.below(i + 1));
        }
    }
}

struct Sent {
    facts: FrameFacts,
    unit: Vec<u8>,
    packets: Vec<Vec<u8>>,
    data: usize,
}

fn facts(number: u32, idr: bool, survives_loss: bool) -> FrameFacts {
    FrameFacts {
        number,
        idr,
        survives_loss,
        hevc: false,
        captured: 1_000_000 + u64::from(number),
        encoded: 2_000_000 + u64::from(number),
    }
}

// A frame of `data` data shards at 20 percent parity.
fn frame_of(facts: FrameFacts, data: usize, percent: u32) -> Sent {
    let len = data * SHARD - FRAME_HEADER - 7;
    let unit: Vec<u8> = (0..len).map(|i| (i as u32 ^ facts.number) as u8).collect();
    let mut packetizer = Packetizer::new(PAYLOAD).unwrap();
    let packets = packetizer
        .packetize(&facts, &unit, percent)
        .unwrap()
        .iter()
        .map(<[u8]>::to_vec)
        .collect();
    Sent {
        facts,
        unit,
        packets,
        data,
    }
}

fn frame(number: u32, data: usize) -> Sent {
    frame_of(facts(number, false, true), data, 20)
}

#[derive(Debug, PartialEq, Eq)]
enum Out {
    Frame(u32),
    Repaired(u32),
    Dropped(u32, u32, DropReason),
    Recover(u32, u32),
}

use DropReason::{Deadline, Memory, Overtaken, Refused};

// Checks every frame's bytes against what was sent.
struct Viewer {
    reassembler: Reassembler,
    sent: Vec<(FrameFacts, Vec<u8>)>,
}

impl Viewer {
    fn new(interval: Duration) -> Viewer {
        Viewer {
            reassembler: Reassembler::new(interval),
            sent: Vec::new(),
        }
    }

    fn knows(&mut self, sent: &Sent) {
        self.sent.push((sent.facts, sent.unit.clone()));
    }

    fn push(&mut self, packet: &[u8], now: Instant) -> (Arrival, Vec<Out>) {
        let arrival = self.reassembler.push(packet, now);
        (arrival, self.drain())
    }

    fn expire(&mut self, now: Instant) -> Vec<Out> {
        self.reassembler.expire(now);
        self.drain()
    }

    fn drain(&mut self) -> Vec<Out> {
        let mut out = Vec::new();
        while let Some(event) = self.reassembler.event() {
            out.push(match event {
                Event::Frame(frame) => {
                    let number = frame.facts.number;
                    let (facts, unit) = self
                        .sent
                        .iter()
                        .find(|(facts, _)| facts.number == number)
                        .expect("a frame that was never sent came out");
                    assert_eq!(frame.facts, *facts);
                    assert_eq!(frame.access_unit, &unit[..], "frame {number}");
                    if frame.repaired {
                        Out::Repaired(number)
                    } else {
                        Out::Frame(number)
                    }
                }
                Event::Dropped { first, last, why } => Out::Dropped(first, last, why),
                Event::Recover { first, last } => Out::Recover(first, last),
            });
        }
        out
    }

    // Every packet of the frame except the ones listed, all at `now`.
    fn send(&mut self, sent: &Sent, skip: &[usize], now: Instant) -> Vec<Out> {
        self.knows(sent);
        let mut out = Vec::new();
        for (index, packet) in sent.packets.iter().enumerate() {
            if !skip.contains(&index) {
                let (arrival, events) = self.push(packet, now);
                assert!(
                    matches!(arrival, Arrival::Kept | Arrival::Surplus | Arrival::Late),
                    "{arrival:?}"
                );
                out.extend(events);
            }
        }
        out
    }
}

#[test]
fn frames_come_out_in_order() {
    let mut random = Random(0x9E37_79B9_7F4A_7C15);
    let now = Instant::now();
    let mut both = 0;
    let mut first_dropped = 0;
    for round in 0..400 {
        let a = frame(10, 1 + round % 12);
        let b = frame(11, 1 + (round * 7) % 9);
        let mut viewer = Viewer::new(FPS_120);
        assert_eq!(viewer.send(&frame(9, 1), &[], now), [Out::Frame(9)]);
        viewer.knows(&a);
        viewer.knows(&b);
        let mut packets: Vec<(u32, &Vec<u8>)> = a
            .packets
            .iter()
            .map(|packet| (10, packet))
            .chain(b.packets.iter().map(|packet| (11, packet)))
            .collect();
        random.shuffle(&mut packets);
        if round % 4 == 0 {
            // Within each frame only: all of a, then all of b.
            packets.sort_by_key(|&(frame, _)| frame);
        }
        // When a has enough shards before b does, both come out, a first.
        // When b is whole first, a is dropped at that moment.
        let (mut a_seen, mut b_seen) = (0, 0);
        let mut a_first = None;
        let mut out = Vec::new();
        for (frame, packet) in packets {
            if frame == 10 {
                a_seen += 1;
            } else {
                b_seen += 1;
            }
            if a_first.is_none() && (a_seen == a.data || b_seen == b.data) {
                a_first = Some(a_seen == a.data);
            }
            let (arrival, events) = viewer.push(packet, now);
            assert!(
                matches!(arrival, Arrival::Kept | Arrival::Surplus | Arrival::Late),
                "{arrival:?}"
            );
            out.extend(events);
        }
        if a_first == Some(true) {
            both += 1;
            assert!(
                matches!(out[0], Out::Frame(10) | Out::Repaired(10)),
                "round {round}: {out:?}"
            );
            assert!(
                matches!(out[1], Out::Frame(11) | Out::Repaired(11)),
                "{out:?}"
            );
            assert_eq!(out.len(), 2);
        } else {
            first_dropped += 1;
            assert_eq!(
                out[0],
                Out::Dropped(10, 10, Overtaken),
                "round {round}: {out:?}"
            );
            assert!(
                matches!(out[1], Out::Frame(11) | Out::Repaired(11)),
                "{out:?}"
            );
            assert_eq!(out[2], Out::Recover(10, 10));
            assert_eq!(out.len(), 3);
        }
    }
    assert!(both > 100 && first_dropped > 50, "{both} {first_dropped}");
}

#[test]
fn frame_waits_one_interval() {
    let now = Instant::now();
    // 120 fps waits the 10 ms floor, 60 fps its own interval.
    for (interval, wait) in [(FPS_120, WAIT_120), (FPS_60, FPS_60)] {
        let mut viewer = Viewer::new(interval);
        let sent = frame(1, 5);
        // Two packets short of whole: 5 data and 1 parity, 4 arrive.
        assert_eq!(viewer.send(&sent, &[1, 3], now), []);
        assert_eq!(viewer.reassembler.next_deadline(), Some(now + wait));
        assert_eq!(viewer.expire(now + wait - US), []);
        assert_eq!(
            viewer.expire(now + wait),
            [Out::Dropped(1, 1, Deadline), Out::Recover(1, 1)]
        );
        assert_eq!(viewer.reassembler.next_deadline(), None);
        // The rest of it is too late now.
        let (arrival, out) = viewer.push(&sent.packets[1], now + wait);
        assert_eq!((arrival, out), (Arrival::Late, vec![]));
    }

    // The wait starts over with each packet.
    let mut viewer = Viewer::new(FPS_120);
    let sent = frame(1, 5);
    viewer.knows(&sent);
    viewer.push(&sent.packets[0], now);
    viewer.push(&sent.packets[2], now + 6 * MS);
    assert_eq!(viewer.expire(now + 16 * MS - US), []);
    let (arrival, out) = viewer.push(&sent.packets[4], now + 16 * MS - US);
    assert_eq!((arrival, out), (Arrival::Kept, vec![]));
    assert_eq!(viewer.expire(now + 26 * MS - 2 * US), []);
    assert_eq!(
        viewer.expire(now + 26 * MS - US),
        [Out::Dropped(1, 1, Deadline), Out::Recover(1, 1)]
    );

    // A packet that comes after the wait ran out finds its frame gone, even
    // when the caller's timer has not fired yet.
    let mut viewer = Viewer::new(FPS_120);
    let sent = frame(1, 2);
    viewer.knows(&sent);
    viewer.push(&sent.packets[0], now);
    let (arrival, out) = viewer.push(&sent.packets[1], now + WAIT_120);
    assert_eq!(arrival, Arrival::Late);
    assert_eq!(out, [Out::Dropped(1, 1, Deadline), Out::Recover(1, 1)]);
}

#[test]
fn later_ready_frame_drops_the_waiting_ones() {
    let now = Instant::now();
    let mut viewer = Viewer::new(FPS_120);
    let first = frame(1, 4);
    viewer.send(&first, &[0, 1], now);
    // Frame 2 never shows up at all; frame 3 is whole 2 ms later.
    let third = frame(3, 3);
    let out = viewer.send(&third, &[], now + 2 * MS);
    assert_eq!(
        out,
        [
            Out::Dropped(1, 2, Overtaken),
            Out::Frame(3),
            Out::Recover(1, 2)
        ]
    );
    let numbers = viewer.reassembler.numbers();
    assert_eq!((numbers.dropped_overtaken, numbers.delivered), (2, 1));

    // Two frames wait; the later one's time runs out first, since the older
    // one's last packet came later. Both go at that moment.
    let mut viewer = Viewer::new(FPS_120);
    viewer.send(&frame(0, 1), &[], now);
    let (a, b) = (frame(1, 4), frame(2, 4));
    viewer.knows(&a);
    viewer.knows(&b);
    viewer.push(&b.packets[0], now);
    viewer.push(&a.packets[0], now + 3 * MS);
    assert_eq!(viewer.reassembler.next_deadline(), Some(now + WAIT_120));
    assert_eq!(
        viewer.expire(now + WAIT_120),
        [
            Out::Dropped(1, 1, Overtaken),
            Out::Dropped(2, 2, Deadline),
            Out::Recover(1, 2)
        ]
    );
}

#[test]
fn after_a_drop_frames_are_held_until_the_next_idr() {
    let now = Instant::now();
    let mut viewer = Viewer::new(FPS_120);
    let at = |n: u32| now + FPS_120 * n;
    let sent = |number: u32, idr: bool| frame_of(facts(number, idr, false), 3, 20);
    assert_eq!(viewer.send(&sent(1, true), &[], at(1)), [Out::Frame(1)]);
    assert_eq!(viewer.send(&sent(2, false), &[], at(2)), [Out::Frame(2)]);
    // Frame 3 loses two of its four packets.
    assert_eq!(viewer.send(&sent(3, false), &[1, 2], at(3)), []);
    assert_eq!(
        viewer.send(&sent(4, false), &[], at(4)),
        [Out::Dropped(3, 3, Overtaken), Out::Recover(3, 3)]
    );
    assert_eq!(viewer.send(&sent(5, false), &[], at(5)), []);
    assert_eq!(viewer.send(&sent(6, true), &[], at(6)), [Out::Frame(6)]);
    assert_eq!(viewer.send(&sent(7, false), &[], at(7)), [Out::Frame(7)]);
    let numbers = viewer.reassembler.numbers();
    assert_eq!(
        (numbers.delivered, numbers.skipped, numbers.dropped()),
        (4, 2, 1)
    );

    // An IDR that is itself lost is asked for again: frame 8 runs out of
    // time as frame 10 comes, frame 9 is overtaken by it, and the request
    // for 9 waits out the gap after the one for 8.
    assert_eq!(viewer.send(&sent(8, false), &[0, 1], at(8)), []);
    assert_eq!(viewer.send(&sent(9, true), &[0, 2], at(9)), []);
    let out = viewer.send(&sent(10, false), &[], at(10));
    assert_eq!(
        out,
        [
            Out::Dropped(8, 8, Deadline),
            Out::Recover(8, 8),
            Out::Dropped(9, 9, Overtaken)
        ]
    );
    assert_eq!(viewer.send(&sent(11, true), &[3], at(11)), [Out::Frame(11)]);
    assert_eq!(viewer.expire(at(10) + RECOVER_GAP), [Out::Recover(9, 9)]);
}

#[test]
fn invalidation_keeps_frames_coming() {
    let now = Instant::now();
    let mut viewer = Viewer::new(FPS_120);
    let at = |n: u32| now + FPS_120 * n;
    let sent = |number: u32| frame_of(facts(number, number == 1, true), 3, 20);
    assert_eq!(viewer.send(&sent(1), &[], at(1)), [Out::Frame(1)]);
    assert_eq!(viewer.send(&sent(2), &[0, 1], at(2)), []);
    assert_eq!(
        viewer.send(&sent(3), &[], at(3)),
        [
            Out::Dropped(2, 2, Overtaken),
            Out::Frame(3),
            Out::Recover(2, 2)
        ]
    );
    assert_eq!(viewer.send(&sent(4), &[], at(4)), [Out::Frame(4)]);
    assert_eq!(viewer.reassembler.numbers().skipped, 0);

    // A dropped frame's own header decides, when its first shard came: this
    // one says an IDR is needed, though the frames before said otherwise.
    let needs_idr = frame_of(facts(5, false, false), 3, 20);
    assert_eq!(viewer.send(&needs_idr, &[1, 2], at(5)), []);
    assert_eq!(
        viewer.send(&sent(6), &[], at(6)),
        [Out::Dropped(5, 5, Overtaken), Out::Recover(5, 5)]
    );
    assert_eq!(viewer.reassembler.numbers().skipped, 1);
}

// Right after an IDR, a frame lost with its header is guessed from the
// headers of frames that are not IDRs, and on a stream that invalidates the
// sharer invalidates it and the frames after it keep coming. The IDR's own
// header counts for nothing, whatever it says. Frame 2's wait runs out before
// frame 3 comes, so no later header is in when it drops.
#[test]
fn drop_after_an_idr_on_an_invalidating_stream() {
    let now = Instant::now();
    let at = |n: u32| now + FPS_120 * n;
    for idr_says in [true, false] {
        let mut viewer = Viewer::new(FPS_120);
        let sent = |number: u32| frame_of(facts(number, false, true), 3, 20);
        let idr = frame_of(facts(1, true, idr_says), 3, 20);
        assert_eq!(viewer.send(&idr, &[], at(1)), [Out::Frame(1)]);
        // Frame 2 loses its first shard and one more of its four packets.
        assert_eq!(viewer.send(&sent(2), &[0, 1], at(2)), []);
        assert_eq!(
            viewer.expire(at(2) + WAIT_120),
            [Out::Dropped(2, 2, Deadline), Out::Recover(2, 2)]
        );
        let late = at(2) + WAIT_120 + MS;
        assert_eq!(
            viewer.send(&sent(3), &[], late),
            [Out::Frame(3)],
            "the IDR's header says {idr_says}"
        );
        assert_eq!(viewer.send(&sent(4), &[], late + MS), [Out::Frame(4)]);
        assert_eq!(viewer.reassembler.numbers().skipped, 0);
    }
}

// A dropped frame whose own header says IDR holds the frames after it until
// the next IDR, even on a stream that invalidates: nothing before an IDR is
// kept to predict from. An IDR lost with its header cannot be told from any
// other frame, so it is guessed like one, and the viewer's own ask for an
// IDR covers it when nothing decodes.
#[test]
fn a_dropped_idr_holds_until_the_next_idr() {
    let now = Instant::now();
    let at = |n: u32| now + FPS_120 * n;
    let mut viewer = Viewer::new(FPS_120);
    let sent = |number: u32, idr: bool| frame_of(facts(number, idr, true), 3, 20);
    assert_eq!(viewer.send(&sent(1, true), &[], at(1)), [Out::Frame(1)]);
    assert_eq!(viewer.send(&sent(2, false), &[], at(2)), [Out::Frame(2)]);
    // IDR 3 keeps its first shard and loses two of the other three.
    assert_eq!(viewer.send(&sent(3, true), &[1, 2], at(3)), []);
    assert_eq!(
        viewer.send(&sent(4, false), &[], at(4)),
        [Out::Dropped(3, 3, Overtaken), Out::Recover(3, 3)]
    );
    assert_eq!(viewer.send(&sent(5, false), &[], at(5)), []);
    assert_eq!(viewer.send(&sent(6, true), &[], at(6)), [Out::Frame(6)]);
    assert_eq!(viewer.send(&sent(7, false), &[], at(7)), [Out::Frame(7)]);
    let numbers = viewer.reassembler.numbers();
    assert_eq!(
        (numbers.delivered, numbers.skipped, numbers.dropped()),
        (4, 2, 1)
    );

    // IDR 8 lost with its header: the frames after it come out.
    assert_eq!(viewer.send(&sent(8, true), &[0, 1], at(8)), []);
    assert_eq!(
        viewer.send(&sent(9, false), &[], at(9)),
        [
            Out::Dropped(8, 8, Overtaken),
            Out::Frame(9),
            Out::Recover(8, 8)
        ]
    );
}

#[test]
fn frame_numbers_wrap() {
    let now = Instant::now();
    let mut viewer = Viewer::new(FPS_120);
    let start = u32::MAX - 3;
    for (n, number) in (start..=u32::MAX).chain(0..3).enumerate() {
        let skip: &[usize] = if number == u32::MAX { &[0, 1] } else { &[] };
        let out = viewer.send(&frame(number, 2), skip, now + FPS_120 * n as u32);
        match number {
            u32::MAX => assert_eq!(out, []),
            0 => assert_eq!(
                out,
                [
                    Out::Dropped(u32::MAX, u32::MAX, Overtaken),
                    Out::Frame(0),
                    Out::Recover(u32::MAX, u32::MAX)
                ]
            ),
            _ => assert_eq!(out, [Out::Frame(number)]),
        }
    }
    let numbers = viewer.reassembler.numbers();
    assert_eq!(
        (numbers.delivered, numbers.dropped(), numbers.far),
        (6, 1, 0)
    );
}

#[test]
fn duplicates_and_late_packets_are_counted_and_ignored() {
    let now = Instant::now();
    let mut viewer = Viewer::new(FPS_120);
    let sent = frame(1, 3);
    viewer.knows(&sent);
    assert_eq!(viewer.push(&sent.packets[0], now).0, Arrival::Kept);
    assert_eq!(viewer.push(&sent.packets[0], now).0, Arrival::Duplicate);
    assert_eq!(viewer.push(&sent.packets[3], now).0, Arrival::Kept);
    let (arrival, out) = viewer.push(&sent.packets[1], now);
    assert_eq!((arrival, out), (Arrival::Kept, vec![Out::Repaired(1)]));
    // The data shard it did without, after the frame came out, still counts
    // as received, as surplus; the same one again is a duplicate.
    assert_eq!(viewer.push(&sent.packets[2], now).0, Arrival::Surplus);
    assert_eq!(viewer.push(&sent.packets[2], now).0, Arrival::Duplicate);
    let numbers = viewer.reassembler.numbers();
    assert_eq!(
        (
            numbers.duplicates,
            numbers.surplus,
            numbers.late,
            numbers.shards_received,
            numbers.shards_lost
        ),
        (2, 1, 0, 4, 0)
    );
    assert_eq!(viewer.reassembler.loss(now).percent(), Some(0.0));
    // A frame dropped long ago is just late.
    let old = frame(0, 1);
    assert_eq!(viewer.push(&old.packets[0], now).0, Arrival::Late);
    assert_eq!(viewer.reassembler.numbers().late, 1);
}

// With no loss every parity shard comes after its frame is out. That is
// surplus, and late stays for shards of frames that were dropped, so the
// stats panel shows reordering and not the parity.
#[test]
fn surplus_and_late_shards() {
    let now = Instant::now();
    let mut viewer = Viewer::new(FPS_120);
    let mut parity = 0;
    for number in 1..=20u32 {
        let sent = frame(number, 1 + number as usize % 12);
        parity += sent.packets.len() - sent.data;
        let out = viewer.send(&sent, &[], now + FPS_120 * number);
        assert_eq!(out, [Out::Frame(number)]);
    }
    let numbers = viewer.reassembler.numbers();
    assert_eq!(
        (numbers.surplus, numbers.late, numbers.shards_lost),
        (parity as u64, 0, 0)
    );

    // Frame 21 loses two of its three packets and is overtaken; its last
    // packet then comes late.
    let dropped = frame(21, 2);
    let at = now + FPS_120 * 21;
    viewer.send(&dropped, &[0, 2], at);
    viewer.send(&frame(22, 2), &[], at);
    let (arrival, out) = viewer.push(&dropped.packets[2], at + MS);
    assert_eq!((arrival, out), (Arrival::Late, vec![]));
    let numbers = viewer.reassembler.numbers();
    assert_eq!((numbers.surplus, numbers.late), (parity as u64 + 1, 1));
}

#[test]
fn memory_is_bounded_by_frames_and_by_bytes() {
    let now = Instant::now();
    let mut viewer = Viewer::new(FPS_120);
    // One packet of each of five frames; the fifth pushes out the oldest.
    let frames: Vec<Sent> = (1..=MAX_PENDING as u32 + 1).map(|n| frame(n, 3)).collect();
    for (n, sent) in frames.iter().enumerate() {
        viewer.knows(sent);
        let (arrival, out) = viewer.push(&sent.packets[0], now + MS * n as u32);
        assert_eq!(arrival, Arrival::Kept);
        if n < MAX_PENDING {
            assert_eq!(out, []);
        } else {
            assert_eq!(out, [Out::Dropped(1, 1, Memory), Out::Recover(1, 1)]);
        }
    }
    assert_eq!(viewer.reassembler.numbers().dropped_memory, 1);

    // Four frames in play and a first packet for one older than all of them:
    // the oldest is that one, so it goes.
    let mut viewer = Viewer::new(FPS_120);
    viewer.send(&frame(1, 1), &[], now);
    for n in 3..3 + MAX_PENDING as u32 {
        viewer.send(&frame(n, 3), &[1, 2, 3], now);
    }
    let older = frame(2, 3);
    let (arrival, out) = viewer.push(&older.packets[0], now + MS);
    assert_eq!(arrival, Arrival::Late);
    assert_eq!(out, [Out::Dropped(2, 2, Memory), Out::Recover(2, 2)]);
    let later = frame(3, 3);
    assert_eq!(viewer.push(&later.packets[1], now + MS).0, Arrival::Kept);

    // Frames of 2048 data shards, each one short: a frame holds the shards
    // that came, each with its two-byte index, so three fit and the fourth
    // pushes out the first once the bytes held would pass the limit.
    let mut viewer = Viewer::new(FPS_120);
    let big: Vec<Sent> = (1..=4)
        .map(|n| frame_of(facts(n, true, true), 2048, 50))
        .collect();
    let held = 2 + SHARD;
    let fit = (MAX_HELD_BYTES - 3 * 2047 * held) / held;
    assert!(fit < 2047);
    for sent in &big[..3] {
        viewer.knows(sent);
        for packet in &sent.packets[1..2048] {
            assert_eq!(viewer.push(packet, now), (Arrival::Kept, vec![]));
        }
    }
    viewer.knows(&big[3]);
    for packet in &big[3].packets[1..=fit] {
        assert_eq!(viewer.push(packet, now), (Arrival::Kept, vec![]));
    }
    assert_eq!(
        viewer.push(&big[3].packets[fit + 1], now),
        (
            Arrival::Kept,
            vec![Out::Dropped(1, 1, Memory), Out::Recover(1, 1)]
        )
    );
    // The first's missing shard is late now, and the three still held come
    // out whole, their shards put in order.
    assert_eq!(viewer.push(&big[0].packets[0], now).0, Arrival::Late);
    assert_eq!(viewer.push(&big[1].packets[0], now).1, [Out::Frame(2)]);
    assert_eq!(viewer.push(&big[2].packets[0], now).1, [Out::Frame(3)]);
    let out = viewer.send(&big[3], &(1..=fit + 1).collect::<Vec<_>>(), now);
    assert_eq!(out, [Out::Frame(4)]);
    assert_eq!(viewer.reassembler.numbers().dropped_memory, 1);

    // When the oldest frame is the one the shard is for, that frame goes and
    // the shard is late.
    let mut viewer = Viewer::new(FPS_120);
    let shard = [0u8; MAX_SHARD];
    let packet = |frame: u32, index: u16| {
        let mut bytes = Vec::new();
        Packet {
            frame,
            index,
            data: 2048,
            parity: 1,
            shard: &shard,
        }
        .write(&mut bytes);
        bytes
    };
    viewer.push(&packet(1, 0), now);
    for frame in [2, 3, 4] {
        for index in 0..2047 {
            assert_eq!(viewer.push(&packet(frame, index), now).0, Arrival::Kept);
        }
    }
    // Frame 1 takes what room is left, never enough to make it whole.
    let fit = MAX_HELD_BYTES / (2 + MAX_SHARD) - 1 - 3 * 2047;
    assert!(fit < 2047);
    for index in 1..=fit as u16 {
        assert_eq!(viewer.push(&packet(1, index), now), (Arrival::Kept, vec![]));
    }
    assert_eq!(
        viewer.push(&packet(1, fit as u16 + 1), now),
        (
            Arrival::Late,
            vec![Out::Dropped(1, 1, Memory), Out::Recover(1, 1)]
        )
    );
}

#[test]
fn drops_close_together_make_one_recover_request() {
    let now = Instant::now();
    let mut viewer = Viewer::new(FPS_120);
    let at = |n: u32| now + FPS_120 * n;
    viewer.send(&frame(1, 2), &[], at(1));
    // Frames 2 to 4 wait with a shard short, then frame 5 overtakes them:
    // one request for all three.
    for n in 2..=4 {
        viewer.send(&frame(n, 2), &[0, 1], at(2));
    }
    assert_eq!(
        viewer.send(&frame(5, 2), &[], at(3)),
        [
            Out::Dropped(2, 4, Overtaken),
            Out::Frame(5),
            Out::Recover(2, 4)
        ]
    );
    // Drops inside the gap after a request wait for its end and go as one.
    let t = at(3);
    viewer.send(&frame(6, 2), &[0, 1], t + MS);
    assert_eq!(
        viewer.send(&frame(7, 2), &[], t + 2 * MS),
        [Out::Dropped(6, 6, Overtaken), Out::Frame(7)]
    );
    viewer.send(&frame(8, 2), &[1, 2], t + 9 * MS);
    assert_eq!(
        viewer.send(&frame(9, 2), &[], t + 10 * MS),
        [Out::Dropped(8, 8, Overtaken), Out::Frame(9)]
    );
    assert_eq!(viewer.reassembler.next_deadline(), Some(t + RECOVER_GAP));
    assert_eq!(viewer.expire(t + RECOVER_GAP - US), []);
    assert_eq!(viewer.expire(t + RECOVER_GAP), [Out::Recover(6, 8)]);
    assert_eq!(viewer.reassembler.next_deadline(), None);
    // After a quiet gap, a drop is asked about at once again.
    viewer.send(&frame(10, 2), &[0, 1], t + 50 * MS);
    assert_eq!(
        viewer.send(&frame(11, 2), &[], t + 51 * MS),
        [
            Out::Dropped(10, 10, Overtaken),
            Out::Frame(11),
            Out::Recover(10, 10)
        ]
    );
    assert_eq!(viewer.reassembler.numbers().recover_requests, 3);
}

#[test]
fn far_packets_unless_the_stream_moved() {
    let now = Instant::now();
    let mut viewer = Viewer::new(FPS_120);
    viewer.send(&frame(100, 2), &[], now);
    // One stray packet from far ahead, then the stream goes on.
    let stray = frame(100 + 5000, 2);
    assert_eq!(viewer.push(&stray.packets[0], now).0, Arrival::Far);
    assert_eq!(viewer.send(&frame(101, 2), &[], now), [Out::Frame(101)]);
    // From far behind, it is late: the sharer's numbers only go forward.
    let behind = frame(100u32.wrapping_sub(500), 2);
    assert_eq!(viewer.push(&behind.packets[0], now).0, Arrival::Late);
    assert_eq!(viewer.send(&frame(102, 2), &[], now), [Out::Frame(102)]);
    assert_eq!(viewer.reassembler.numbers().far, 1);

    // Two packets in a row from far ahead: the sharer's numbers moved, and
    // the stream follows from the first of them.
    let moved = frame(9000, 2);
    viewer.knows(&moved);
    assert_eq!(viewer.push(&moved.packets[0], now).0, Arrival::Far);
    let (arrival, out) = viewer.push(&moved.packets[1], now);
    assert_eq!(arrival, Arrival::Kept);
    assert_eq!(out, [Out::Recover(103, 8999)]);
    let (_, out) = viewer.push(&moved.packets[2], now);
    assert_eq!(out, [Out::Repaired(9000)]);
    assert_eq!(viewer.send(&frame(9001, 2), &[], now), [Out::Frame(9001)]);
    let numbers = viewer.reassembler.numbers();
    assert_eq!((numbers.far, numbers.restarts), (2, 1));
}

// Packets of one frame from long ago, as a path change can bring: however
// many come, the stream never moves back to them, and nothing in play is
// dropped for them.
#[test]
fn packets_from_long_ago_never_move_the_stream_back() {
    let now = Instant::now();
    for base in [1000u32, 50] {
        let mut viewer = Viewer::new(FPS_120);
        for number in base..base + 3 {
            let out = viewer.send(&frame(number, 2), &[], now);
            assert_eq!(out, [Out::Frame(number)]);
        }
        // Frame base + 3 waits for its last data shard when the old ones
        // come: from 100 frames back, or wrapped past zero.
        let waiting = frame(base + 3, 2);
        viewer.send(&waiting, &[1, 2], now);
        let old = frame(base.wrapping_sub(100), 2);
        for packet in &old.packets {
            let (arrival, out) = viewer.push(packet, now + MS);
            assert_eq!((arrival, out), (Arrival::Late, vec![]), "base {base}");
        }
        let (_, out) = viewer.push(&waiting.packets[1], now + 2 * MS);
        assert_eq!(out, [Out::Frame(base + 3)]);
        let numbers = viewer.reassembler.numbers();
        assert_eq!(
            (
                numbers.restarts,
                numbers.dropped(),
                numbers.far,
                numbers.late
            ),
            (0, 0, 0, 3),
            "base {base}"
        );
    }

    // Before anything is out, a packet from up to 64 frames before the first
    // one heard still moves the start back to it; further back is late.
    let mut viewer = Viewer::new(FPS_120);
    let (first, before, long_before) = (frame(500, 2), frame(436, 2), frame(435, 2));
    viewer.knows(&first);
    viewer.knows(&before);
    viewer.push(&first.packets[0], now);
    assert_eq!(viewer.push(&long_before.packets[0], now).0, Arrival::Late);
    assert_eq!(viewer.push(&before.packets[0], now).0, Arrival::Kept);
    assert_eq!(viewer.push(&before.packets[1], now).1, [Out::Frame(436)]);
}

#[test]
fn mismatched_packet_drops_its_frame() {
    let now = Instant::now();
    let mut viewer = Viewer::new(FPS_120);
    let sent = frame(1, 3);
    viewer.knows(&sent);
    viewer.push(&sent.packets[0], now);
    let good = read_packet(&sent.packets[1]).unwrap();
    // The header and the access unit are 3 * 1152 - 7 = 3449 bytes; over
    // three shards and rounded up to a whole step, 1152 each.
    let shard = good.shard.len();
    assert_eq!(shard, SHARD);
    let mut bad = Vec::new();
    Packet { parity: 2, ..good }.write(&mut bad);
    let (arrival, out) = viewer.push(&bad, now);
    assert_eq!(
        arrival,
        Arrival::Refused(PacketError::Mismatch {
            data: 3,
            parity: 2,
            shard,
            first: (3, 1, shard)
        })
    );
    assert_eq!(out, [Out::Dropped(1, 1, Refused), Out::Recover(1, 1)]);
    assert_eq!(viewer.push(&sent.packets[1], now).0, Arrival::Late);

    // The same counts at another shard length, one the frame could have
    // had at another size or on the LAN: the frame goes all the same.
    let mut viewer = Viewer::new(FPS_120);
    viewer.knows(&sent);
    viewer.push(&sent.packets[0], now);
    let mut longer = good.shard.to_vec();
    longer.resize(MAX_SHARD, 0);
    let mut bad = Vec::new();
    Packet {
        shard: &longer,
        ..good
    }
    .write(&mut bad);
    let (arrival, out) = viewer.push(&bad, now);
    assert_eq!(
        arrival,
        Arrival::Refused(PacketError::Mismatch {
            data: 3,
            parity: 1,
            shard: MAX_SHARD,
            first: (3, 1, shard)
        })
    );
    assert_eq!(out, [Out::Dropped(1, 1, Refused), Out::Recover(1, 1)]);

    // Behind a frame still waiting, it goes when its turn comes.
    let mut viewer = Viewer::new(FPS_120);
    let (a, b) = (frame(1, 3), frame(2, 3));
    viewer.knows(&a);
    viewer.knows(&b);
    viewer.push(&a.packets[0], now);
    viewer.push(&b.packets[0], now);
    let mut bad = Vec::new();
    Packet {
        shard: &[0; MIN_SHARD],
        ..read_packet(&b.packets[1]).unwrap()
    }
    .write(&mut bad);
    let (arrival, out) = viewer.push(&bad, now);
    assert!(matches!(
        arrival,
        Arrival::Refused(PacketError::Mismatch { .. })
    ));
    assert_eq!(out, []);
    let out = viewer.send(&a, &[0], now + MS);
    assert_eq!(
        out,
        [
            Out::Frame(1),
            Out::Dropped(2, 2, Refused),
            Out::Recover(2, 2)
        ]
    );
    let numbers = viewer.reassembler.numbers();
    assert_eq!((numbers.protocol_errors, numbers.dropped_refused), (1, 1));
}

// Every packet agrees, on a shard longer than the packetizer cuts for the
// frame: 100 bytes padded out to a whole 1152-byte shard, where Booth sends
// one of 512. Refused as a whole and asked for again, not shown.
#[test]
fn a_frame_in_longer_shards_than_booth_cuts_is_refused() {
    let mut shard = vec![0; SHARD];
    shard[0] = 0b10;
    shard[17..21].copy_from_slice(&100u32.to_le_bytes());
    shard[FRAME_HEADER..FRAME_HEADER + 100].fill(7);
    let mut packet = Vec::new();
    Packet {
        frame: 1,
        index: 0,
        data: 1,
        parity: 1,
        shard: &shard,
    }
    .write(&mut packet);
    let mut viewer = Viewer::new(FPS_120);
    let (arrival, out) = viewer.push(&packet, Instant::now());
    assert_eq!(arrival, Arrival::Kept);
    assert_eq!(out, [Out::Dropped(1, 1, Refused), Out::Recover(1, 1)]);
    let numbers = viewer.reassembler.numbers();
    assert_eq!((numbers.protocol_errors, numbers.dropped_refused), (1, 1));
}

#[test]
fn loss_is_counted_from_shards_over_two_seconds() {
    let now = Instant::now();
    let mut viewer = Viewer::new(FPS_120);
    assert_eq!(viewer.reassembler.loss(now).percent(), None);
    // Ten frames of five data and one parity shard, one packet lost from
    // each: 10 of 60.
    for n in 0..10u32 {
        viewer.send(&frame(n, 5), &[n as usize % 6], now + FPS_120 * n);
    }
    let at = now + FPS_120 * 10;
    viewer.send(&frame(10, 5), &[], at);
    let loss = viewer.reassembler.loss(at);
    assert_eq!((loss.lost, loss.expected), (10, 66));
    // Two frames never seen count as two average frames, all lost.
    viewer.send(&frame(13, 5), &[], at + MS);
    let loss = viewer.reassembler.loss(at + MS);
    assert_eq!((loss.lost, loss.expected), (22, 84));
    assert_eq!(viewer.reassembler.numbers().shards_lost, 22);
    // Two seconds later it is all forgotten.
    assert_eq!(
        viewer
            .reassembler
            .loss(at + MS + Duration::from_secs(2))
            .percent(),
        None
    );
}

#[test]
fn the_parity_rule() {
    assert_eq!(parity_percent(None), 20);
    assert_eq!(parity_percent(Some(0.0)), 20);
    assert_eq!(parity_percent(Some(5.0)), 20);
    assert_eq!(parity_percent(Some(9.9)), 20);
    assert_eq!(parity_percent(Some(10.0)), 20);
    assert_eq!(parity_percent(Some(10.2)), 21);
    assert_eq!(parity_percent(Some(12.5)), 25);
    assert_eq!(parity_percent(Some(24.9)), 50);
    assert_eq!(parity_percent(Some(100.0)), 50);
    assert_eq!(parity_percent(Some(-1.0)), 20);
    assert_eq!(parity_percent(Some(f32::NAN)), 20);
    assert_eq!(parity_percent(Some(f32::INFINITY)), 20);
}

// The first packet heard need not be from the first frame sent: until a
// frame is out, one from a little before it moves the start back.
#[test]
fn stream_starts_at_the_oldest_frame_heard() {
    let now = Instant::now();
    let mut viewer = Viewer::new(FPS_120);
    let (a, b) = (frame(20, 2), frame(21, 2));
    viewer.knows(&a);
    viewer.knows(&b);
    assert_eq!(viewer.push(&b.packets[0], now).0, Arrival::Kept);
    assert_eq!(viewer.push(&a.packets[0], now).0, Arrival::Kept);
    assert_eq!(viewer.push(&a.packets[1], now).1, [Out::Frame(20)]);
    assert_eq!(viewer.push(&b.packets[1], now).1, [Out::Frame(21)]);
    // Once one is out, the frames before it are over.
    let old = frame(19, 2);
    assert_eq!(viewer.push(&old.packets[0], now).0, Arrival::Late);

    // Not so far back that a frame already in play would be out of reach.
    let mut viewer = Viewer::new(FPS_120);
    let (first, far) = (frame(2000, 2), frame(2000 + 1020, 2));
    viewer.knows(&first);
    viewer.knows(&far);
    assert_eq!(viewer.push(&first.packets[0], now).0, Arrival::Kept);
    assert_eq!(viewer.push(&far.packets[0], now).0, Arrival::Kept);
    let before = frame(1990, 2);
    assert_eq!(viewer.push(&before.packets[0], now).0, Arrival::Late);
    assert_eq!(
        viewer.expire(now + WAIT_120),
        [
            Out::Dropped(2000, 3019, Overtaken),
            Out::Dropped(3020, 3020, Deadline),
            Out::Recover(2000, 3020)
        ]
    );
}

// A frame dropped before any header said what the encoder does after a loss:
// the next header decides, so an encoder that invalidates references is not
// left waiting for an IDR it never sends.
#[test]
fn drop_before_any_header() {
    let now = Instant::now();
    for (survives, expected) in [(true, vec![Out::Frame(2)]), (false, vec![])] {
        let mut viewer = Viewer::new(FPS_120);
        let first = frame_of(facts(1, true, survives), 3, 20);
        // Its first shard never arrives, and neither does enough of the rest.
        viewer.send(&first, &[0, 1], now);
        let out = viewer.send(&frame_of(facts(2, false, survives), 3, 20), &[], now + MS);
        let mut all = vec![Out::Dropped(1, 1, Overtaken)];
        all.extend(expected);
        all.push(Out::Recover(1, 1));
        assert_eq!(out, all, "survives loss: {survives}");
        let later = viewer.send(
            &frame_of(facts(3, false, survives), 3, 20),
            &[],
            now + 2 * MS,
        );
        let skipped = viewer.reassembler.numbers().skipped;
        if survives {
            assert_eq!((later, skipped), (vec![Out::Frame(3)], 0));
        } else {
            assert_eq!((later, skipped), (vec![], 2));
        }
    }
}

// Whole frames of a stream that jumps around the way a hostile or broken
// sharer's could, packets in any order, time moving in uneven steps: frames
// come out byte for byte and in increasing order, even where the stream
// follows the numbers far ahead, and every call returns. A stream that
// jumps ahead and then carries on behind is late from then on, so not every
// seed gets frames out.
#[test]
fn jumping_stream() {
    let mut seeds_with_frames = 0;
    for seed in 1..=30u64 {
        let mut random = Random(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let base = [0u32, 1000, u32::MAX - 500][seed as usize % 3];
        let mut viewer = Viewer::new(FPS_120);
        let offsets: Vec<u32> = (0..40)
            .map(|_| match random.below(10) {
                0 => base.wrapping_sub(random.below(100) as u32),
                1 => base.wrapping_add(1000 + random.below(100) as u32),
                2 => base.wrapping_add(5000 + random.below(3) as u32),
                _ => base.wrapping_add(random.below(40) as u32),
            })
            .collect();
        let mut frames: Vec<Sent> = Vec::new();
        for number in offsets {
            if frames.iter().all(|sent| sent.facts.number != number) {
                let sent = frame_of(
                    facts(number, random.below(4) == 0, random.below(2) == 0),
                    1 + random.below(6),
                    20,
                );
                viewer.knows(&sent);
                frames.push(sent);
            }
        }
        let mut packets: Vec<&Vec<u8>> =
            frames.iter().flat_map(|sent| sent.packets.iter()).collect();
        // Reordered a few packets deep, across frame boundaries.
        for chunk in packets.chunks_mut(6) {
            random.shuffle(chunk);
        }
        let mut now = Instant::now();
        let mut last_out: Option<u32> = None;
        for packet in packets {
            now += US * random.below(4000) as u32;
            let (_, mut out) = viewer.push(packet, now);
            if random.below(3) == 0
                && let Some(deadline) = viewer.reassembler.next_deadline()
            {
                now = now.max(deadline);
                out.extend(viewer.expire(now));
            }
            for out in out {
                if let Out::Frame(number) | Out::Repaired(number) = out {
                    if let Some(last) = last_out {
                        assert!(
                            number.wrapping_sub(last).wrapping_sub(1) < 1 << 31,
                            "{last} then {number}"
                        );
                    }
                    last_out = Some(number);
                }
            }
        }
        viewer.expire(now + Duration::from_secs(1));
        assert_eq!(viewer.reassembler.next_deadline(), None);
        let numbers = viewer.reassembler.numbers();
        assert_eq!(numbers.protocol_errors, 0);
        if numbers.delivered + numbers.skipped > 0 {
            seeds_with_frames += 1;
        }
    }
    println!("{seeds_with_frames} of 30 jumping streams got frames out");
    assert!(seeds_with_frames >= 15, "{seeds_with_frames}");
}
