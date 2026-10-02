use std::time::{Duration, Instant};

use channels::video::{
    Arrival, Event, FRAME_HEADER, FrameFacts, HEADER, MAX_DATA, MAX_SHARD, MIN_SHARD,
    PARITY_CEILING, PARITY_DEFAULT, PARITY_FLOOR, Packet, PacketError, PacketizeError, Packetizer,
    Reassembler, SHARD_STEP, lost_for_good, parity_count, parity_for_loss, parity_percent,
    read_packet,
};
use proptest::prelude::*;

// 1200-byte datagrams less 32 bytes of session overhead, the channel byte and
// the room's two bytes, and the largest shard that leaves after the packet
// header, in whole steps.
const PAYLOAD: usize = 1200 - 32 - 1 - 2;
const SHARD: usize = 1152;
const FPS_120: Duration = Duration::from_nanos(8_333_333);

// xorshift64*, seeded, so a failure shows up the same way every run.
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

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            items.swap(i, self.below(i + 1));
        }
    }
}

fn facts(number: u32) -> FrameFacts {
    FrameFacts {
        number,
        idr: number.is_multiple_of(3),
        survives_loss: true,
        // Every other frame, so the codec is checked on the way through too.
        hevc: number.is_multiple_of(2),
        captured: 1_790_284_323_456_789 + u64::from(number) * 8_333,
        encoded: 1_790_284_323_460_000 + u64::from(number) * 8_333,
    }
}

fn packets(
    packetizer: &mut Packetizer,
    facts: &FrameFacts,
    unit: &[u8],
    percent: u32,
) -> Vec<Vec<u8>> {
    packetizer
        .packetize(facts, unit, percent)
        .unwrap()
        .iter()
        .map(<[u8]>::to_vec)
        .collect()
}

// Owned copies of what came out, for comparing.
#[derive(Debug, PartialEq)]
enum Out {
    Frame {
        facts: FrameFacts,
        unit: Vec<u8>,
        repaired: bool,
    },
    Dropped(u32, u32),
    Recover(u32, u32),
}

fn drain(reassembler: &mut Reassembler) -> Vec<Out> {
    let mut out = Vec::new();
    while let Some(event) = reassembler.event() {
        out.push(match event {
            Event::Frame(frame) => Out::Frame {
                facts: frame.facts,
                unit: frame.access_unit.to_vec(),
                repaired: frame.repaired,
            },
            Event::Dropped { first, last, .. } => Out::Dropped(first, last),
            Event::Recover { first, last } => Out::Recover(first, last),
        });
    }
    out
}

fn push_all<'a>(
    reassembler: &mut Reassembler,
    packets: impl IntoIterator<Item = &'a Vec<u8>>,
    now: Instant,
) -> Vec<Out> {
    let mut out = Vec::new();
    for packet in packets {
        assert_eq!(reassembler.push(packet, now), Arrival::Kept);
        out.extend(drain(reassembler));
    }
    out
}

#[test]
fn packet_layout() {
    let mut packetizer = Packetizer::new(PAYLOAD).unwrap();
    assert_eq!(packetizer.largest_shard(), SHARD);
    assert_eq!(packetizer.largest_access_unit(), 2048 * 1152 - 21);
    let facts = FrameFacts {
        number: 0x0403_0201,
        idr: true,
        survives_loss: true,
        hevc: true,
        captured: 0x1817_1615_1413_1211,
        encoded: 0x2827_2625_2423_2221,
    };
    let unit = [0xAA; 1200];
    let sent = packetizer.packetize(&facts, &unit, 20).unwrap();
    // Two shards of 1152 at most, so two of 640: the frame header and 1200
    // bytes are 1221, over two and rounded up to a multiple of 64.
    assert_eq!((sent.data(), sent.parity(), sent.len()), (2, 1, 3));
    assert_eq!(sent.packet_len(), HEADER + 640);
    let first = sent.get(0).unwrap();
    // frame 1-4, index 0, 2 data, 1 parity
    assert_eq!(first[..10], [1, 2, 3, 4, 0, 0, 2, 0, 1, 0]);
    // The frame header: flags (IDR, survives loss and HEVC), captured,
    // encoded, then the access unit's length, 1200.
    assert_eq!(first[10], 0b111);
    assert_eq!(
        first[11..19],
        [0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18]
    );
    assert_eq!(
        first[19..27],
        [0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28]
    );
    assert_eq!(first[27..31], [0xB0, 0x04, 0, 0]);
    assert!(first[31..].iter().all(|&byte| byte == 0xAA));
    let second = sent.get(1).unwrap();
    assert_eq!(second[4..6], [1, 0]);
    // 640 - 21 bytes went in the first shard, the other 581 in the second,
    // then 59 zeros.
    assert!(second[10..10 + 581].iter().all(|&byte| byte == 0xAA));
    assert_eq!(second[10 + 581..], [0; 59]);
    assert_eq!(sent.get(2).unwrap()[4..6], [2, 0]);
    assert_eq!(sent.get(3), None);

    let read = read_packet(second).unwrap();
    assert_eq!(
        (
            read.frame,
            read.index,
            read.data,
            read.parity,
            read.shard.len()
        ),
        (0x0403_0201, 1, 2, 1, 640)
    );
    let mut again = Vec::new();
    read.write(&mut again);
    assert_eq!(again, second);

    // Any frame of up to 491 bytes: one shard of the smallest length, and
    // its parity as big. One byte more is a step longer.
    for len in [1, 300, 491] {
        let sent = packetizer.packetize(&facts, &vec![0xAA; len], 20).unwrap();
        assert_eq!((sent.len(), sent.packet_len()), (2, HEADER + MIN_SHARD));
    }
    let sent = packetizer.packetize(&facts, &[0xAA; 492], 20).unwrap();
    assert_eq!((sent.len(), sent.packet_len()), (2, HEADER + 576));
    // One that fills its shards exactly keeps the largest.
    let sent = packetizer
        .packetize(&facts, &[0xAA; 3 * 1152 - 21], 20)
        .unwrap();
    assert_eq!((sent.data(), sent.packet_len()), (3, HEADER + SHARD));
    assert!(sent.packet_len() <= PAYLOAD);
}

#[test]
fn every_size_round_trips() {
    let mut packetizer = Packetizer::new(PAYLOAD).unwrap();
    let shard = packetizer.largest_shard();
    let mut random = Random(0x9E37_79B9_7F4A_7C15);
    let largest = packetizer.largest_access_unit();
    // One byte, a frame that fills its shards exactly, one byte over, and
    // the largest there is.
    let sizes = [
        1,
        shard - FRAME_HEADER,
        shard - FRAME_HEADER + 1,
        3 * shard - FRAME_HEADER,
        5000,
        83_333,
        largest,
    ];
    let mut reassembler = Reassembler::new(FPS_120);
    let now = Instant::now();
    for (number, len) in sizes.into_iter().enumerate() {
        let unit = random.bytes(len);
        let facts = facts(number as u32);
        let sent = packets(&mut packetizer, &facts, &unit, 20);
        let data = (FRAME_HEADER + len).div_ceil(shard);
        assert_eq!(
            sent.len(),
            data + parity_count(data as u16, 20) as usize,
            "{len} bytes"
        );
        let out = push_all(&mut reassembler, &sent[..data], now);
        assert_eq!(
            out,
            [Out::Frame {
                facts,
                unit,
                repaired: false
            }],
            "{len} bytes"
        );
    }
    assert_eq!(reassembler.numbers().delivered, sizes.len() as u64);
}

#[test]
fn packetizer_refuses_what_does_not_fit() {
    let mut packetizer = Packetizer::new(PAYLOAD).unwrap();
    let largest = packetizer.largest_access_unit();
    assert_eq!(
        packetizer.packetize(&facts(1), &[], 20).unwrap_err(),
        PacketizeError::Empty
    );
    let too_big = vec![1; largest + 1];
    assert_eq!(
        packetizer.packetize(&facts(1), &too_big, 20).unwrap_err(),
        PacketizeError::TooBig {
            len: largest + 1,
            largest
        }
    );
    // The largest frame is 2048 data shards and, at 50 percent, 1024 parity.
    let sent = packetizer.packetize(&facts(1), &too_big[1..], 50).unwrap();
    assert_eq!((sent.data(), sent.parity()), (MAX_DATA, 1024));

    // A packet smaller than its header and the smallest shard, and one that
    // leaves a shard past the largest.
    for payload in [
        0,
        HEADER,
        HEADER + MIN_SHARD - 1,
        HEADER + MAX_SHARD + SHARD_STEP,
        usize::MAX,
    ] {
        assert_eq!(
            Packetizer::new(payload).unwrap_err(),
            PacketizeError::Payload(payload)
        );
    }
    assert_eq!(
        PacketizeError::Payload(8).to_string(),
        "a video packet of 8 bytes; Booth's packets hold a 10-byte header and a shard of 512 \
         to 1344 bytes in steps of 64"
    );
    // Bytes short of a whole step are left unused.
    assert_eq!(
        Packetizer::new(HEADER + MIN_SHARD).unwrap().largest_shard(),
        MIN_SHARD
    );
    assert_eq!(
        Packetizer::new(HEADER + MIN_SHARD + SHARD_STEP - 1)
            .unwrap()
            .largest_shard(),
        MIN_SHARD
    );
    assert_eq!(Packetizer::new(1400).unwrap().largest_shard(), MAX_SHARD);
    // On a LAN: 1400-byte datagrams.
    assert_eq!(
        Packetizer::new(1400 - 32 - 1 - 2).unwrap().largest_shard(),
        1344
    );
    assert_eq!(MAX_SHARD, 1344);
}

#[test]
fn parity_count_rounds_up() {
    assert_eq!(parity_count(1, 20), 1);
    assert_eq!(parity_count(1, 10), 1);
    assert_eq!(parity_count(1, 0), 1);
    assert_eq!(parity_count(2, 50), 1);
    assert_eq!(parity_count(3, 50), 2);
    assert_eq!(parity_count(5, 20), 1);
    assert_eq!(parity_count(6, 20), 2);
    assert_eq!(parity_count(10, 10), 1);
    assert_eq!(parity_count(11, 10), 2);
    assert_eq!(parity_count(72, 20), 15);
    assert_eq!(parity_count(430, 20), 86);
    assert_eq!(parity_count(2048, 50), 1024);
    assert_eq!(parity_count(7, 100), 7);
    assert_eq!(parity_count(7, 400), 7);
    assert_eq!(parity_count(2048, u32::MAX), 2048);
}

// Every term of the sum, as products, for frames small enough that none of
// them is under what an f64 holds.
fn lost_for_good_by_hand(data: u16, parity: u16, loss: f64) -> f64 {
    let n = u32::from(data) + u32::from(parity);
    (u32::from(parity) + 1..=n)
        .map(|k| {
            let ways = (0..k).fold(1.0, |ways, i| ways * f64::from(n - i) / f64::from(i + 1));
            ways * loss.powi(k as i32) * (1.0 - loss).powi((n - k) as i32)
        })
        .sum()
}

// Why a percentage alone is not enough: at 5 percent loss one parity shard
// leaves a frame of 3, 4 or 5 data shards lost for good 1.4, 2.3 and 3.3
// times in 100.
#[test]
fn lost_for_good_known_values() {
    let in_100 = |data, parity| lost_for_good(data, parity, 0.05) * 100.0;
    assert!((in_100(3, 1) - 1.40).abs() < 0.005, "{}", in_100(3, 1));
    assert!((in_100(4, 1) - 2.26).abs() < 0.005, "{}", in_100(4, 1));
    assert!((in_100(5, 1) - 3.28).abs() < 0.005, "{}", in_100(5, 1));
    assert!((in_100(5, 2) - 0.38).abs() < 0.005, "{}", in_100(5, 2));
    // One data and one parity shard: both lost.
    assert!((lost_for_good(1, 1, 0.2) - 0.04).abs() < 1e-12);
    assert_eq!(lost_for_good(10, 2, 0.0), 0.0);
    assert_eq!(lost_for_good(10, 2, -0.5), 0.0);
    assert_eq!(lost_for_good(10, 2, f64::NAN), 0.0);
    assert_eq!(lost_for_good(10, 2, 1.0), 1.0);
    assert_eq!(lost_for_good(0, 2, 0.5), 0.0);
    // The largest frame, whose terms as products are all under an f64's
    // smallest: at the ceiling 20 percent loss is 18 deviations short of
    // it, at the floor 4 past it.
    let ceiling = lost_for_good(MAX_DATA, 1024, 0.2);
    assert!((0.0..1e-30).contains(&ceiling), "{ceiling}");
    let floor = lost_for_good(MAX_DATA, 410, 0.2);
    assert!(floor > 0.9999 && floor <= 1.0, "{floor}");
}

// Each frame gets the fewest parity shards that keep its chance of being lost
// for good under 1 in 100 at the loss the viewer reports, never fewer than
// the 20 percent floor gives and never more than the ceiling; before any
// report, the default.
#[test]
fn parity_for_loss_is_the_fewest_that_will_do() {
    for data in 1..=MAX_DATA {
        assert_eq!(
            parity_for_loss(data, None),
            parity_count(data, PARITY_DEFAULT)
        );
        assert_eq!(
            parity_for_loss(data, Some(0.0)),
            parity_count(data, PARITY_FLOOR)
        );
        for percent in [
            0.5f32, 1.0, 2.5, 5.0, 7.5, 10.0, 15.0, 20.0, 30.0, 40.0, 75.0, 100.0,
        ] {
            let parity = parity_for_loss(data, Some(percent));
            let least = parity_count(data, PARITY_FLOOR);
            let most = parity_count(data, PARITY_CEILING);
            assert!(
                (least..=most).contains(&parity),
                "{data} data shards at {percent} percent: {parity}"
            );
            let loss = f64::from(percent) / 100.0;
            if parity < most {
                assert!(
                    lost_for_good(data, parity, loss) < 0.01,
                    "{data} data shards at {percent} percent: {parity}"
                );
            }
            if parity > least {
                assert!(
                    lost_for_good(data, parity - 1, loss) >= 0.01,
                    "{data} data shards at {percent} percent: {parity}"
                );
            }
        }
    }
    let at = |percent: f32| -> Vec<u16> {
        (1..=20)
            .map(|data| parity_for_loss(data, Some(percent)))
            .collect()
    };
    // At 5 percent a frame of 3 to 5 or 8 to 10 data shards gets one more
    // than the floor gives, and no frame two more.
    assert_eq!(
        at(5.0),
        [1, 1, 2, 2, 2, 2, 2, 3, 3, 3, 3, 3, 3, 3, 4, 4, 4, 4, 4, 4]
    );
    assert_eq!(
        at(1.0),
        (1..=20)
            .map(|data| parity_count(data, 20))
            .collect::<Vec<_>>()
    );
    // Past 10 percent twice the loss is more than a big frame needs: at 20
    // percent 500 data shards stay under 1 in 100 with 155, not 200.
    assert_eq!(parity_count(500, parity_percent(Some(20.0))), 200);
    assert_eq!(parity_for_loss(500, Some(20.0)), 155);
}

// A table of parity shards by data shards and loss, with the chance of a
// frame lost for good, and in parentheses the percentage alone where the rule
// differs. Then what working it out costs a frame, at the losses a link has
// and at those only a hostile viewer would report.
#[test]
fn parity_for_the_loss_table() {
    let losses = [1.0f32, 5.0, 10.0, 20.0];
    println!(
        "data shards: parity and lost for good at 1, 5, 10 and 20 percent loss (the \
         percentage alone)"
    );
    let sizes = (1..=20).chain([25, 30, 40, 50, 66, 72, 78, 100, 200, 500, 1000, 1188, 2048]);
    for data in sizes {
        let cells: Vec<String> = losses
            .iter()
            .map(|&percent| {
                let loss = f64::from(percent) / 100.0;
                let rule = parity_for_loss(data, Some(percent));
                let alone = parity_count(data, parity_percent(Some(percent)));
                let lost = lost_for_good(data, rule, loss) * 100.0;
                if rule == alone {
                    format!("{rule:>4} {lost:>5.2}%")
                } else {
                    let was = lost_for_good(data, alone, loss) * 100.0;
                    format!("{rule:>4} {lost:>5.2}% ({alone} {was:.2}%)")
                }
            })
            .collect();
        println!("{data:>5}: {}", cells.join("  "));
    }

    let build = if cfg!(debug_assertions) {
        "debug build"
    } else {
        "release build"
    };
    // Each size's time is the quickest of three, so a thread switch in the
    // middle of one does not count as the size's.
    let cost = |percents: &[f32]| {
        let (mut total, mut slowest, mut calls) = (Duration::ZERO, Duration::ZERO, 0);
        for data in 1..=MAX_DATA {
            for &percent in percents {
                let quickest = (0..3)
                    .map(|_| {
                        let start = Instant::now();
                        std::hint::black_box(parity_for_loss(
                            std::hint::black_box(data),
                            Some(percent),
                        ));
                        start.elapsed()
                    })
                    .min()
                    .unwrap_or_default();
                total += quickest;
                slowest = slowest.max(quickest);
                calls += 1;
            }
        }
        (total / calls, slowest)
    };
    let (average, slowest) = cost(&[1.0, 5.0, 10.0, 15.0, 20.0, 25.0]);
    println!(
        "{build}: parity_for_loss takes {average:?} a frame on average over every size at 1 to \
         25 percent, {slowest:?} for the slowest"
    );
    let (average, slowest) = cost(&[30.0, 35.0, 50.0, 75.0, 99.0, 1e30]);
    println!("{build}: and {average:?} on average from 30 percent up, {slowest:?} for the slowest");
}

// A frame of four data shards: one parity shard before any report, two at
// 5 percent and still two, the ceiling, at 20. Any two lost, it comes out
// whole.
#[test]
fn packetizer_takes_parity_from_the_loss() {
    let mut packetizer = Packetizer::new(PAYLOAD).unwrap();
    let mut random = Random(0x6A09_E667_F3BC_C908);
    let unit = random.bytes(4 * SHARD - FRAME_HEADER - 100);
    for (loss, parity) in [(None, 1), (Some(0.0), 1), (Some(5.0), 2), (Some(20.0), 2)] {
        let sent = packetizer
            .packetize_for_loss(&facts(1), &unit, loss)
            .unwrap();
        assert_eq!((sent.data(), sent.parity()), (4, parity), "{loss:?}");
    }
    let sent: Vec<Vec<u8>> = packetizer
        .packetize_for_loss(&facts(1), &unit, Some(5.0))
        .unwrap()
        .iter()
        .map(<[u8]>::to_vec)
        .collect();
    for first in 0..sent.len() {
        for second in first + 1..sent.len() {
            let mut reassembler = Reassembler::new(FPS_120);
            let out = arrive(&mut reassembler, &sent, &[first, second], Instant::now());
            assert_eq!(
                out,
                [Out::Frame {
                    facts: facts(1),
                    unit: unit.clone(),
                    repaired: first < 4
                }],
                "lost {first} and {second}"
            );
        }
    }
}

fn good_packet() -> Vec<u8> {
    let mut packetizer = Packetizer::new(PAYLOAD).unwrap();
    packets(&mut packetizer, &facts(7), &[5; 3000], 20).swap_remove(1)
}

#[test]
fn a_bad_field_is_refused_and_says_which() {
    let good = good_packet();
    assert!(read_packet(&good).is_ok());
    let with = |at: usize, value: u16| {
        let mut bad = good.clone();
        bad[at..at + 2].copy_from_slice(&value.to_le_bytes());
        read_packet(&bad).map(|_| ())
    };
    // Offsets: frame 0, index 4, data 6, parity 8, shard 10.
    assert_eq!(with(6, 0), Err(PacketError::DataCount(0)));
    assert_eq!(with(6, 2049), Err(PacketError::DataCount(2049)));
    assert_eq!(
        with(8, 0),
        Err(PacketError::ParityCount { parity: 0, data: 3 })
    );
    assert_eq!(
        with(8, 4),
        Err(PacketError::ParityCount { parity: 4, data: 3 })
    );
    assert_eq!(with(4, 4), Err(PacketError::Index { index: 4, total: 4 }));
    assert_eq!(
        with(4, u16::MAX),
        Err(PacketError::Index {
            index: u16::MAX,
            total: 4
        })
    );
    assert!(with(4, 3).is_ok());

    for cut in 0..HEADER {
        assert_eq!(read_packet(&good[..cut]), Err(PacketError::Short(cut)));
    }
    assert_eq!(read_packet(&good[..HEADER]), Err(PacketError::Shard(0)));
    // The good one is 3000 bytes and the header over three shards, 1007
    // each rounded up to 1024. Shorter by an odd byte or an even two is not
    // a whole number of steps; one step under the smallest and one over the
    // largest are.
    assert_eq!(good.len(), HEADER + 1024);
    for cut in [1, 2] {
        assert_eq!(
            read_packet(&good[..good.len() - cut]),
            Err(PacketError::Shard(1024 - cut))
        );
    }
    assert_eq!(
        read_packet(&good[..HEADER + MIN_SHARD - SHARD_STEP]),
        Err(PacketError::Shard(MIN_SHARD - SHARD_STEP))
    );
    assert!(read_packet(&good[..HEADER + MIN_SHARD]).is_ok());
    let mut long = good.clone();
    long.resize(HEADER + MAX_SHARD + SHARD_STEP, 0);
    assert_eq!(
        read_packet(&long),
        Err(PacketError::Shard(MAX_SHARD + SHARD_STEP))
    );
    long.truncate(HEADER + MAX_SHARD);
    assert!(read_packet(&long).is_ok());

    // Each refusal reads as a sentence for the log.
    assert_eq!(
        PacketError::ParityCount { parity: 4, data: 3 }.to_string(),
        "4 parity shards for 3 data shards; Booth sends 1 to as many as the data"
    );
    assert_eq!(
        PacketError::Shard(1154).to_string(),
        "a shard of 1154 bytes; Booth sends a multiple of 64 from 512 to 1344"
    );
}

// Whatever a friend's PC sends, the reassembler counts what it refuses and
// never panics, and anything it lets out is a frame the format allows.
fn check_counts(reassembler: &mut Reassembler, packets: &[Vec<u8>], now: Instant) {
    let before = reassembler.numbers().protocol_errors;
    let mut refused = 0;
    for packet in packets {
        if let Arrival::Refused(err) = reassembler.push(packet, now) {
            refused += 1;
            assert!(!err.to_string().is_empty());
        }
        while let Some(event) = reassembler.event() {
            if let Event::Frame(frame) = event {
                assert!(!frame.access_unit.is_empty());
            }
        }
    }
    reassembler.expire(now + Duration::from_secs(1));
    drain(reassembler);
    // Refusals from packets plus frames refused as a whole.
    assert!(reassembler.numbers().protocol_errors - before >= refused);
}

#[test]
fn edited_packets_never_panic() {
    let mut packetizer = Packetizer::new(PAYLOAD).unwrap();
    let seeds: Vec<Vec<Vec<u8>>> = [(1u32, 1usize, 20u32), (2, 3000, 50), (3, 20_000, 10)]
        .into_iter()
        .map(|(number, len, percent)| {
            packets(&mut packetizer, &facts(number), &vec![9; len], percent)
        })
        .collect();
    let mut random = Random(0xD1B5_4A32_D192_ED03);
    let now = Instant::now();
    for round in 0..300 {
        let mut reassembler = Reassembler::new(FPS_120);
        let mut frame = seeds[round % seeds.len()].clone();
        for _ in 0..1 + random.below(3) {
            let packet = random.below(frame.len());
            let at = random.below(frame[packet].len());
            frame[packet][at] = random.next() as u8;
        }
        random.shuffle(&mut frame);
        check_counts(&mut reassembler, &frame, now);
    }
}

proptest! {
    #[test]
    fn random_bytes_are_refused_or_counted(
        packets in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..1400), 1..20)
    ) {
        let mut reassembler = Reassembler::new(FPS_120);
        check_counts(&mut reassembler, &packets, Instant::now());
    }

    // One field of a good packet set to anything: the packet is refused with
    // a count, or it is taken and whatever follows is still sound.
    #[test]
    fn one_field_changed(field in 0usize..6, value in any::<u32>(), which in 0usize..4) {
        let mut packetizer = Packetizer::new(PAYLOAD).unwrap();
        let mut frame = packets(&mut packetizer, &facts(9), &[3; 3000], 20);
        let packet = &mut frame[which];
        match field {
            0 => packet[0..4].copy_from_slice(&value.to_le_bytes()),
            1 => packet[4..6].copy_from_slice(&(value as u16).to_le_bytes()),
            2 => packet[6..8].copy_from_slice(&(value as u16).to_le_bytes()),
            3 => packet[8..10].copy_from_slice(&(value as u16).to_le_bytes()),
            // Shorter or longer: any length, and a whole number of steps,
            // which can look like a length Booth sends, since the shard
            // length is the frame's own.
            4 => packet.resize(HEADER + (value as usize % 1400), 0),
            _ => packet.resize(HEADER + SHARD_STEP * (value as usize % 23), 0),
        }
        let mut reassembler = Reassembler::new(FPS_120);
        check_counts(&mut reassembler, &frame, Instant::now());
    }

    // Packets of a good frame cut short or padded to other lengths Booth
    // sends, in any order, as a sharer mixing up two frames' lengths would
    // send them: a frame comes out only as it was sent, and a changed length
    // is always counted as a protocol error.
    #[test]
    fn packets_cut_to_other_lengths(
        len in 1usize..6000,
        recut in prop::collection::vec(
            (0usize..16, MIN_SHARD / SHARD_STEP..=MAX_SHARD / SHARD_STEP),
            1..4,
        ),
        seed in any::<u64>(),
    ) {
        let mut packetizer = Packetizer::new(PAYLOAD).unwrap();
        let unit = vec![3; len];
        let sent = packets(&mut packetizer, &facts(9), &unit, 20);
        let mut frame = sent.clone();
        // Each cut from the packet as sent, so one cut back to its own
        // length is that packet and not one with its end zeroed, which no
        // length check could see.
        for (which, steps) in recut {
            let which = which % frame.len();
            frame[which].clone_from(&sent[which]);
            frame[which].resize(HEADER + SHARD_STEP * steps, 0);
        }
        let changed = frame.iter().any(|packet| packet.len() != sent[0].len());
        Random(seed | 1).shuffle(&mut frame);
        let mut reassembler = Reassembler::new(FPS_120);
        let now = Instant::now();
        let mut out = Vec::new();
        for packet in &frame {
            reassembler.push(packet, now);
            out.extend(drain(&mut reassembler));
        }
        reassembler.expire(now + Duration::from_secs(1));
        out.extend(drain(&mut reassembler));
        for event in out {
            if let Out::Frame { unit: got, .. } = event {
                prop_assert_eq!(&got, &unit);
            }
        }
        prop_assert_eq!(reassembler.numbers().protocol_errors > 0, changed);
    }
}

// Which of a frame's packets arrive: every set that loses at most as many as
// there is parity, then sets that lose one more.
fn arrive(
    reassembler: &mut Reassembler,
    sent: &[Vec<u8>],
    lost: &[usize],
    now: Instant,
) -> Vec<Out> {
    let mut out = Vec::new();
    for (index, packet) in sent.iter().enumerate() {
        if !lost.contains(&index) {
            reassembler.push(packet, now);
            out.extend(drain(reassembler));
        }
    }
    reassembler.expire(now + Duration::from_secs(1));
    out.extend(drain(reassembler));
    out
}

fn check_parity(len: usize, percent: u32, lost: &[usize], random: &mut Random) {
    let mut packetizer = Packetizer::new(PAYLOAD).unwrap();
    let unit = random.bytes(len);
    let facts = facts(1);
    let sent = packets(&mut packetizer, &facts, &unit, percent);
    let mut reassembler = Reassembler::new(FPS_120);
    let out = arrive(&mut reassembler, &sent, lost, Instant::now());
    let parity = sent.len() - (FRAME_HEADER + len).div_ceil(packetizer.largest_shard());
    let data_lost = lost.iter().any(|&index| index < sent.len() - parity);
    if lost.len() == sent.len() {
        // Nothing arrived, so nothing is known of the frame.
        assert_eq!(out, []);
    } else if lost.len() <= parity {
        assert_eq!(
            out,
            [Out::Frame {
                facts,
                unit,
                repaired: data_lost
            }],
            "{len} bytes at {percent} percent, lost {lost:?}"
        );
    } else {
        assert_eq!(
            out,
            [Out::Dropped(1, 1), Out::Recover(1, 1)],
            "lost {lost:?}"
        );
    }
}

#[test]
fn loss_up_to_the_parity_is_repaired() {
    let mut random = Random(0x2545_F491_4F6C_DD1D);
    // One shard, exactly two shards, and frames of 5, 72 and 430 shards.
    let shard = SHARD;
    for (len, percent) in [
        (100, 20),
        (2 * shard - FRAME_HEADER, 20),
        (2 * shard - FRAME_HEADER, 50),
        (5 * shard - 400, 20),
        (72 * shard - 500, 20),
        (72 * shard - 500, 10),
        (430 * shard - 30, 20),
    ] {
        let data = (FRAME_HEADER + len).div_ceil(shard);
        let parity = usize::from(parity_count(data as u16, percent));
        let total = data + parity;
        for round in 0..12 {
            // Lose exactly `parity` shards, then one more; the first rounds
            // lose only data, the last only the first data shard and parity.
            let mut order: Vec<usize> = (0..total).collect();
            random.shuffle(&mut order);
            if round == 0 {
                order.sort();
            }
            if round == 1 {
                order.sort_by_key(|&index| std::cmp::Reverse(index));
            }
            let lost = &order[..parity];
            check_parity(len, percent, lost, &mut random);
            let lost = &order[..parity + 1];
            check_parity(len, percent, lost, &mut random);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn any_set_of_lost_shards_up_to_the_parity(
        data in 1usize..40,
        percent in 10u32..=50,
        seed in any::<u64>(),
        extra in any::<bool>(),
    ) {
        let mut random = Random(seed | 1);
        let len = data * SHARD - FRAME_HEADER - random.below(1100);
        let data = (FRAME_HEADER + len).div_ceil(SHARD);
        let parity = usize::from(parity_count(data as u16, percent));
        let mut order: Vec<usize> = (0..data + parity).collect();
        random.shuffle(&mut order);
        let lost = random.below(parity + 1) + usize::from(extra);
        check_parity(len, percent, &order[..lost.min(data + parity)], &mut random);
    }

    // Any payload a room could pass and any length: as many packets as the
    // largest shard needs, each shard the shortest length in whole steps
    // that carries the frame in that many, and the frame whole again with
    // as many lost as there is parity.
    #[test]
    fn shortest_shard_for_the_count(
        payload in HEADER + MIN_SHARD..HEADER + MAX_SHARD + SHARD_STEP,
        len in 1usize..20_000,
        seed in any::<u64>(),
    ) {
        let mut packetizer = Packetizer::new(payload).unwrap();
        let largest = packetizer.largest_shard();
        let unit = Random(seed | 1).bytes(len);
        let facts = facts(5);
        let sent = packets(&mut packetizer, &facts, &unit, 20);
        let data = (FRAME_HEADER + len).div_ceil(largest);
        let parity = usize::from(parity_count(data as u16, 20));
        prop_assert_eq!(sent.len(), data + parity);
        let shard = sent[0].len() - HEADER;
        prop_assert!(sent.iter().all(|packet| packet.len() == HEADER + shard));
        let room = payload - HEADER;
        prop_assert!(largest.is_multiple_of(SHARD_STEP) && largest <= room);
        prop_assert!(room < largest + SHARD_STEP);
        prop_assert!(shard.is_multiple_of(SHARD_STEP) && (MIN_SHARD..=largest).contains(&shard));
        prop_assert!(data * shard >= FRAME_HEADER + len);
        prop_assert!(shard == MIN_SHARD || data * (shard - SHARD_STEP) < FRAME_HEADER + len);
        let lost: Vec<usize> = (0..parity).collect();
        let mut reassembler = Reassembler::new(FPS_120);
        let out = arrive(&mut reassembler, &sent, &lost, Instant::now());
        prop_assert_eq!(out, vec![Out::Frame { facts, unit, repaired: true }]);
    }

    // Any number a viewer could report, a hostile one included: the floor's
    // parity to the ceiling's, the fewest in between that keeps a frame
    // under 1 in 100, and never less for more loss.
    #[test]
    fn any_reported_loss(
        data in 1..=MAX_DATA,
        loss in prop_oneof![any::<f32>(), 0.0f32..100.0],
        other in any::<f32>(),
    ) {
        let parity = parity_for_loss(data, Some(loss));
        if !loss.is_finite() {
            prop_assert_eq!(parity, parity_count(data, PARITY_DEFAULT));
        }
        let (least, most) = (parity_count(data, PARITY_FLOOR), parity_count(data, PARITY_CEILING));
        prop_assert!((least..=most).contains(&parity));
        if loss.is_finite() {
            let loss = f64::from(loss.clamp(0.0, 100.0)) / 100.0;
            prop_assert!(parity == most || lost_for_good(data, parity, loss) < 0.01);
            prop_assert!(parity == least || lost_for_good(data, parity - 1, loss) >= 0.01);
        }
        if loss.is_finite() && other.is_finite() {
            let (less, more) = if loss <= other { (loss, other) } else { (other, loss) };
            prop_assert!(parity_for_loss(data, Some(less)) <= parity_for_loss(data, Some(more)));
        }
    }

    #[test]
    fn lost_for_good_matches_the_sum(
        data in 1u16..60,
        parity in 0u16..60,
        loss in 0.001f64..0.999,
    ) {
        let by_hand = lost_for_good_by_hand(data, parity, loss);
        let summed = lost_for_good(data, parity, loss);
        prop_assert!(
            (summed - by_hand).abs() <= by_hand * 1e-9 + 1e-300,
            "{} against {} by hand", summed, by_hand
        );
    }
}

fn median(mut times: Vec<Duration>) -> Duration {
    times.sort();
    times[times.len() / 2]
}

#[test]
fn parity_encode_and_repair_time() {
    let mut packetizer = Packetizer::new(PAYLOAD).unwrap();
    let mut random = Random(0x5851_F42D_4C95_7F2D);
    let build = if cfg!(debug_assertions) {
        "debug build"
    } else {
        "release build"
    };
    for (shards, percent) in [(10, 20), (72, 20), (430, 20)] {
        let unit = random.bytes(shards * SHARD - FRAME_HEADER);
        let parity = usize::from(parity_count(shards as u16, percent));
        let (mut encode, mut repair) = (Vec::new(), Vec::new());
        let mut reassembler = Reassembler::new(FPS_120);
        for number in 0..20 {
            let start = Instant::now();
            let sent = packetizer
                .packetize(&facts(number), &unit, percent)
                .unwrap();
            encode.push(start.elapsed());
            assert_eq!(sent.data() as usize, shards);
            // The first `parity` data shards lost: the push that completes
            // the frame rebuilds all of them.
            let sent: Vec<Vec<u8>> = sent.iter().map(<[u8]>::to_vec).collect();
            let now = Instant::now();
            let (last, rest) = sent[parity..].split_last().unwrap();
            for packet in rest {
                reassembler.push(packet, now);
            }
            let start = Instant::now();
            reassembler.push(last, now);
            repair.push(start.elapsed());
            let Some(Event::Frame(frame)) = reassembler.event() else {
                panic!("the frame did not come out");
            };
            assert!(frame.repaired && frame.access_unit == &unit[..]);
        }
        println!(
            "{shards} data shards with {parity} parity ({percent} percent), {build}, median of 20: \
             packetize {:?}, repair of {parity} lost data shards {:?}",
            median(encode),
            median(repair),
        );
    }
}

// A frame that is not a Booth frame, though every packet is: flags nobody
// sets. It is refused as a whole and asked for again.
#[test]
fn bad_frame_header_drops_the_frame() {
    let mut shard = vec![0; MIN_SHARD];
    shard[0] = 0x80;
    shard[17..21].copy_from_slice(&10u32.to_le_bytes());
    let packet = |index: u16| {
        let mut bytes = Vec::new();
        Packet {
            frame: 4,
            index,
            data: 1,
            parity: 1,
            shard: &shard,
        }
        .write(&mut bytes);
        bytes
    };
    let mut reassembler = Reassembler::new(FPS_120);
    let now = Instant::now();
    assert_eq!(reassembler.push(&packet(0), now), Arrival::Kept);
    assert_eq!(
        drain(&mut reassembler),
        [Out::Dropped(4, 4), Out::Recover(4, 4)]
    );
    assert_eq!(reassembler.push(&packet(1), now), Arrival::Late);
    let numbers = reassembler.numbers();
    assert_eq!((numbers.protocol_errors, numbers.dropped_refused), (1, 1));
}
