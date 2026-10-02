// Once the buffers have grown for the biggest frame, neither side allocates
// per frame or per packet, whatever the sizes and shard lengths after it.
// Counted per thread, since the tests in this file run side by side.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use channels::video::{
    Arrival, Event, FRAME_HEADER, FrameFacts, MAX_DATA, MAX_PENDING, MAX_SHARD, MIN_SHARD, Packet,
    Packetizer, Reassembler, SHARD_STEP,
};

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
    static ALLOCATED: Cell<u64> = const { Cell::new(0) };
}

fn count(bytes: usize) {
    let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
    let _ = ALLOCATED.try_with(|n| n.set(n.get() + bytes as u64));
}

fn allocations() -> u64 {
    ALLOCATIONS.with(Cell::get)
}

fn allocated() -> u64 {
    ALLOCATED.with(Cell::get)
}

// SAFETY: every call is passed straight to the system allocator with the
// same arguments; counting touches only thread-local integers.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        // SAFETY: the caller's contract for alloc, passed on unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        // SAFETY: as above.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count(new_size);
        // SAFETY: as above.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as above.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

const PAYLOAD: usize = 1200 - 32 - 1 - 2;
const SHARD: usize = 1152;
const INTERVAL: Duration = Duration::from_nanos(8_333_333);

fn facts(number: u32) -> FrameFacts {
    FrameFacts {
        number,
        idr: number.is_multiple_of(50),
        survives_loss: true,
        hevc: false,
        captured: u64::from(number),
        encoded: u64::from(number) + 1,
    }
}

// Sizes from one to 72 shards at 10 to 50 percent, the largest first. Every
// third frame is 100 to 500 bytes in one shard of 512 or 576, like the
// pattern's frames, far shorter than the frames around it.
fn shape(number: u32) -> (usize, u32) {
    if number == 0 {
        return (72 * SHARD - FRAME_HEADER, 50);
    }
    let percent = 10 + (number * 13) % 41;
    if number % 3 == 1 {
        return (100 + (number as usize * 37) % 401, percent);
    }
    let data = 1 + (number as usize * 7919) % 72;
    (
        data * SHARD - FRAME_HEADER - (number as usize % 100),
        percent,
    )
}

#[test]
fn packetizer_does_not_allocate_once_warm() {
    let mut packetizer = Packetizer::new(PAYLOAD).unwrap();
    let unit = vec![7u8; 72 * SHARD];
    let mut packets = 0;
    for number in 0..100 {
        let (len, percent) = shape(number);
        packetizer
            .packetize(&facts(number), &unit[..len], percent)
            .unwrap();
    }
    let before = allocations();
    for number in 100..700 {
        let (len, percent) = shape(number);
        let sent = packetizer
            .packetize(&facts(number), &unit[..len], percent)
            .unwrap();
        packets += sent.len();
    }
    assert_eq!(allocations() - before, 0, "over {packets} packets");
}

#[test]
fn reassembler_does_not_allocate_once_warm() {
    let mut packetizer = Packetizer::new(PAYLOAD).unwrap();
    let unit = vec![7u8; 72 * SHARD];
    let mut reassembler = Reassembler::new(INTERVAL);
    let start = Instant::now();
    // Each frame loses as many packets as it has parity, from the front,
    // so every one is repaired, and every tenth loses one more and is
    // dropped when the next one is ready. Frames are copied out of the
    // packetizer first, so only the reassembler's own allocations count.
    let mut frame = Vec::new();
    let mut run = |number: u32, count: bool| -> u64 {
        let (len, percent) = shape(number);
        let sent = packetizer
            .packetize(&facts(number), &unit[..len], percent)
            .unwrap();
        frame.clear();
        frame.extend(sent.iter().map(|packet| packet.to_vec()));
        let lost = usize::from(sent.parity()) + usize::from(number % 10 == 5);
        let at = start + INTERVAL * number;
        let before = allocations();
        let mut out = 0;
        for packet in &frame[lost..] {
            reassembler.push(packet, at);
            while let Some(event) = reassembler.event() {
                if let Event::Frame(frame) = event {
                    out += frame.access_unit.len() as u64;
                }
            }
        }
        let allocated = allocations() - before;
        assert!(
            !count || allocated == 0,
            "frame {number} allocated {allocated} times"
        );
        out
    };
    // More than 2 s of frames, so the loss window is as long as it gets.
    for number in 0..300 {
        run(number, false);
    }
    let mut bytes = 0;
    for number in 300..900 {
        bytes += run(number, true);
    }
    let numbers = reassembler.numbers();
    assert!(
        numbers.repaired > 700 && numbers.dropped() > 80,
        "{numbers:?}"
    );
    assert!(bytes > 0);
}

// A friend's PC sending packets that each claim a new frame of the largest
// size the format allows, 5.5 MB. A frame holds only the shard that came, so
// nothing is allocated or zeroed per packet, and each new frame past the
// fourth pushes out the oldest.
#[test]
fn largest_frame_claims_cost_their_own_bytes() {
    let shard = vec![0x5a; MAX_SHARD];
    let mut packet = Vec::new();
    let mut reassembler = Reassembler::new(INTERVAL);
    let start = Instant::now();
    let mut claim = |number: u32| {
        packet.clear();
        Packet {
            frame: number,
            index: (number % 4096) as u16,
            data: MAX_DATA,
            parity: MAX_DATA,
            shard: &shard,
        }
        .write(&mut packet);
        // A millisecond apart, so the 2 s loss window fills and stays full.
        let at = start + Duration::from_millis(u64::from(number));
        assert_eq!(reassembler.push(&packet, at), Arrival::Kept);
        while reassembler.event().is_some() {}
    };
    for number in 0..3000 {
        claim(number);
    }
    let before = allocations();
    let took = Instant::now();
    for number in 3000..4000 {
        claim(number);
    }
    let took = took.elapsed();
    assert_eq!(allocations() - before, 0);
    println!(
        "1000 packets each claiming a new frame of {} bytes: {:?} a packet",
        2 * usize::from(MAX_DATA) * MAX_SHARD,
        took / 1000
    );
    let numbers = reassembler.numbers();
    assert_eq!(
        numbers.dropped_memory,
        4000 - MAX_PENDING as u64,
        "{numbers:?}"
    );
}

struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

// Packets for frames of sizes from all over what the format allows, several
// frames in play at once. Buffers sized by what packets claimed once
// allocated tens of times the bytes that came for some of these streams;
// now what is allocated follows what came.
#[test]
fn allocation_follows_the_bytes_that_came() {
    let shard = vec![0x5a; MAX_SHARD];
    let mut packet = Vec::new();
    let mut worst = 0.0f64;
    for seed in 1..=60u64 {
        let mut random = Random(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let sizes: Vec<(u16, u16, usize)> = (0..4)
            .map(|_| {
                if random.below(3) == 0 {
                    return (MAX_DATA, MAX_DATA, MAX_SHARD);
                }
                let data = 1 + random.below(u64::from(MAX_DATA)) as u16;
                let parity = 1 + random.below(u64::from(data)) as u16;
                let steps = random.below(((MAX_SHARD - MIN_SHARD) / SHARD_STEP + 1) as u64);
                (data, parity, MIN_SHARD + SHARD_STEP * steps as usize)
            })
            .collect();
        // Sized up front, so only the reassembler allocates.
        let mut claimed = HashMap::with_capacity(4096);
        let mut reassembler = Reassembler::new(INTERVAL);
        let start = Instant::now();
        let mut next = 0u32;
        let (mut came, mut before) = (0u64, 0u64);
        for n in 0..3000u64 {
            if n == 1000 {
                came = 0;
                before = allocated();
            }
            let frame = next.wrapping_add(random.below(6) as u32);
            if random.below(4) == 0 {
                next = next.wrapping_add(1 + random.below(3) as u32);
            }
            let (data, parity, len) = *claimed
                .entry(frame)
                .or_insert_with(|| sizes[random.below(4) as usize]);
            packet.clear();
            Packet {
                frame,
                index: random.below(u64::from(data) + u64::from(parity)) as u16,
                data,
                parity,
                shard: &shard[..len],
            }
            .write(&mut packet);
            came += packet.len() as u64;
            reassembler.push(&packet, start + Duration::from_micros(n * 200));
            while reassembler.event().is_some() {}
        }
        let ratio = (allocated() - before) as f64 / came as f64;
        worst = worst.max(ratio);
        assert!(
            ratio <= 1.0,
            "seed {seed}: {} bytes allocated for {came} that came, sizes {sizes:?}",
            allocated() - before
        );
    }
    println!(
        "random claims over 60 streams: at most {worst:.2} bytes allocated per byte that came"
    );
}

#[test]
fn count_sees_allocations() {
    let before = allocations();
    let reassembler = Reassembler::new(INTERVAL);
    assert!(allocations() > before);
    drop(reassembler);
}
