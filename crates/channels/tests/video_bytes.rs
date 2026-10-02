// Bytes on the wire for the frame sizes a share sends, three ways: every
// shard at the largest size, as the first packetizer cut them; each frame in
// the shortest even shards that carry it, as shares went out until the
// rounding; and those rounded up to a multiple of 64 and at least 512 bytes,
// as now, so the sizes tell someone on the path little.
// Every datagram also carries 35 bytes around its packet: 32 of the session,
// the channel byte and the room's two-byte prefix. Ten seconds at 120 fps
// and 20 percent parity, what a share sends with no loss.

use std::collections::HashSet;

use channels::video::{
    FRAME_HEADER, FrameFacts, HEADER, MIN_SHARD, Packetizer, parity_count, parity_percent,
};

const AROUND: usize = 32 + 1 + 2;
const INTERNET: usize = 1200 - AROUND;
const LAN: usize = 1400 - AROUND;
const FPS: usize = 120;
const FRAMES: usize = 10 * FPS;
const PERCENT: u32 = 20;
// The mixes' frame sizes are counted in shards of the internet path's largest
// from before the rounding, so every cut below is of the same frames as then.
const MIX_SHARD: usize = 1154;

struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn between(&mut self, low: usize, high: usize) -> usize {
        low + (self.next() % (high - low + 1) as u64) as usize
    }
}

// Any length that takes `data` shards of MIX_SHARD.
fn in_shards(random: &mut Random, data: usize) -> usize {
    random.between(
        (data - 1) * MIX_SHARD - FRAME_HEADER + 1,
        data * MIX_SHARD - FRAME_HEADER,
    )
}

// The test pattern at 2560x1440 through NVENC at 15 Mbit/s, from
// booth.exe --loopback --pattern runs of 1, 5 and 10 s: a first IDR of 1188
// data shards, then frames of about 400 bytes, one shard each (two packets
// a frame from the first second on).
fn pattern(random: &mut Random, number: usize) -> usize {
    if number == 0 {
        return 1188 * MIX_SHARD - FRAME_HEADER - 500;
    }
    random.between(300, 500)
}

// The same pattern through NVENC's HEVC, about 4.3 Mbit/s at 120 fps: frames
// of about 4.5 KB after its IDR, four or five shards each in 1200-byte
// datagrams and three or four in 1400-byte ones.
fn pattern_hevc(random: &mut Random) -> usize {
    random.between(3_900, 5_100)
}

// The loss sweep's mix (tests/video_loss.rs), a 1440p game: an IDR of 66 to
// 78 shards every 2 s, 2 to 20 shards otherwise.
fn game(random: &mut Random, number: usize) -> usize {
    let data = if number.is_multiple_of(240) {
        random.between(66, 78)
    } else {
        random.between(2, 20)
    };
    in_shards(random, data)
}

// The even cut of a frame of `len` bytes, as shares went out before the
// rounding, and its bytes on the wire: the largest even shard the payload
// left, as many as the frame needs, each then the shortest even length from
// 22 bytes that carries it in that many.
fn even_cut(payload: usize, len: usize) -> usize {
    let largest = (payload - HEADER) & !1;
    let data = (FRAME_HEADER + len).div_ceil(largest);
    let shard = (FRAME_HEADER + len)
        .div_ceil(data)
        .next_multiple_of(2)
        .max(FRAME_HEADER + 1);
    let packets = data + usize::from(parity_count(data as u16, PERCENT));
    packets * (HEADER + shard + AROUND)
}

#[derive(Debug, Default)]
struct Bytes {
    video: usize,
    full: usize,
    even: usize,
    rounded: usize,
    packets: usize,
    // Packet lengths sent, each counted once.
    lengths: usize,
}

fn measure(payload: usize, sizes: &[usize]) -> Bytes {
    let mut packetizer = Packetizer::new(payload).unwrap();
    let largest = packetizer.largest_shard();
    let unit = vec![0x5a; sizes.iter().copied().max().unwrap_or(0)];
    let mut bytes = Bytes::default();
    let mut lengths = HashSet::new();
    for &len in sizes {
        let sent = packetizer
            .packetize(&FrameFacts::default(), &unit[..len], PERCENT)
            .unwrap();
        let data = (FRAME_HEADER + len).div_ceil(largest);
        assert_eq!(
            sent.len(),
            data + usize::from(parity_count(data as u16, PERCENT))
        );
        let full = sent.len() * (HEADER + largest + AROUND);
        let rounded = sent.len() * (sent.packet_len() + AROUND);
        assert!(rounded <= full, "{len} bytes");
        lengths.insert(sent.packet_len());
        bytes.video += len;
        bytes.full += full;
        bytes.even += even_cut(payload, len);
        bytes.rounded += rounded;
        bytes.packets += sent.len();
    }
    bytes.lengths = lengths.len();
    bytes
}

fn mbits(bytes: usize, frames: usize) -> f64 {
    (bytes * 8 * FPS) as f64 / frames as f64 / 1e6
}

fn report(name: &str, sizes: &[usize]) -> [Bytes; 2] {
    [INTERNET, LAN].map(|payload| {
        let bytes = measure(payload, sizes);
        let frames = sizes.len();
        println!(
            "{name}, {}-byte datagrams: video {:.2} Mbit/s; on the wire {:.2} with every shard \
             the largest, {:.2} cut to fit in even lengths, {:.2} rounded \
             ({:+.1} percent on even lengths); {} packets of {} lengths",
            payload + AROUND,
            mbits(bytes.video, frames),
            mbits(bytes.full, frames),
            mbits(bytes.even, frames),
            mbits(bytes.rounded, frames),
            bytes.rounded as f64 * 100.0 / bytes.even as f64 - 100.0,
            bytes.packets,
            bytes.lengths,
        );
        bytes
    })
}

#[test]
fn rounded_shard_cost() {
    let mut random = Random(0x9E37_79B9_7F4A_7C15);
    let sizes: Vec<usize> = (0..FRAMES)
        .map(|number| pattern(&mut random, number))
        .collect();
    let [internet, _] = report("the pattern, first IDR included", &sizes);
    // In shards of 1152 the IDR takes 1190 and 238 parity, two more data
    // shards than in shards of 1154.
    assert_eq!(internet.packets, 1428 + 2 * (FRAMES - 1));
    // After the IDR: frames of 300 to 500 bytes, which in even lengths went
    // out in about a hundred different lengths. Under 492 bytes they all go
    // in the smallest shard, the rest one step up, and it still costs under
    // half of a whole datagram each.
    assert!(sizes[1..].iter().any(|&len| len > MIN_SHARD - FRAME_HEADER));
    for bytes in report("the pattern after its IDR", &sizes[1..]) {
        assert_eq!(bytes.lengths, 2, "{bytes:?}");
        assert!(bytes.even < bytes.rounded, "{bytes:?}");
        assert!(bytes.rounded * 2 < bytes.full, "{bytes:?}");
    }

    let sizes: Vec<usize> = (0..FRAMES)
        .map(|number| game(&mut random, number))
        .collect();
    for bytes in report("a 1440p game", &sizes) {
        // Only part of the padding a frame had in its last shard goes, and
        // the packets stay as many.
        assert!(bytes.rounded < bytes.full, "{bytes:?}");
        assert!(bytes.rounded * 100 > bytes.full * 90, "{bytes:?}");
        assert!(bytes.even < bytes.rounded, "{bytes:?}");
    }
}

// Bytes on the wire for these frames with the percentage for `loss` alone,
// the same for every frame, and with each frame's parity from the loss, and
// the most packets the second added to one frame, if it added any.
fn with_parity_for_loss(
    payload: usize,
    sizes: &[usize],
    loss: Option<f32>,
) -> (usize, usize, usize) {
    let mut packetizer = Packetizer::new(payload).unwrap();
    let unit = vec![0x5a; sizes.iter().copied().max().unwrap_or(0)];
    let (mut alone, mut rule, mut most) = (0, 0, 0);
    for &len in sizes {
        let before = packetizer
            .packetize(&FrameFacts::default(), &unit[..len], parity_percent(loss))
            .unwrap();
        let packets = before.len();
        alone += packets * (before.packet_len() + AROUND);
        let after = packetizer
            .packetize_for_loss(&FrameFacts::default(), &unit[..len], loss)
            .unwrap();
        rule += after.len() * (after.packet_len() + AROUND);
        most = most.max(after.len().saturating_sub(packets));
    }
    (alone, rule, most)
}

// What each frame's parity from the loss costs against the percentage alone,
// at the losses a viewer reports: nothing before a report or at 1 percent,
// at 5 percent at most one packet a frame, and never less up to 10 percent,
// where the percentage is the floor. Past 10 percent a frame of 71 data
// shards or more can take less than twice the loss, which is only printed.
#[test]
fn parity_for_loss_cost() {
    let mut random = Random(0x3C6E_F372_FE94_F82B);
    let mixes: [(&str, Vec<usize>); 3] = [
        (
            "the pattern in H.264 after its IDR",
            (1..FRAMES)
                .map(|number| pattern(&mut random, number))
                .collect(),
        ),
        (
            "the pattern in HEVC",
            (0..FRAMES).map(|_| pattern_hevc(&mut random)).collect(),
        ),
        (
            "a 1440p game",
            (0..FRAMES)
                .map(|number| game(&mut random, number))
                .collect(),
        ),
    ];
    for (name, sizes) in &mixes {
        for payload in [INTERNET, LAN] {
            let mut line = format!(
                "{name}, {}-byte datagrams, Mbit/s on the wire with the percentage alone and \
                 with the rule:",
                payload + AROUND
            );
            for loss in [None, Some(1.0), Some(5.0), Some(10.0), Some(20.0)] {
                let (alone, rule, most) = with_parity_for_loss(payload, sizes, loss);
                let at = loss.map_or(String::from("before a report"), |loss| {
                    format!("at {loss}%")
                });
                line += &format!(
                    " {at} {:.2} and {:.2} ({:+.2}, at most {most} more a frame);",
                    mbits(alone, sizes.len()),
                    mbits(rule, sizes.len()),
                    mbits(rule, sizes.len()) - mbits(alone, sizes.len()),
                );
                if loss.is_none_or(|loss| loss <= 10.0) {
                    assert!(rule >= alone, "{name} {at}");
                }
                if loss.is_none_or(|loss| loss <= 1.0) {
                    assert_eq!(rule, alone, "{name} {at}");
                }
                if loss == Some(5.0) {
                    assert!(most <= 1, "{name} {at}");
                }
            }
            println!("{line}");
        }
    }
}
