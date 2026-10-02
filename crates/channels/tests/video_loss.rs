// What a share is held to: 5 percent loss drops under 1 frame in 100, and
// 20 percent keeps the picture moving because the parity follows the loss. A
// sharer and a viewer on virtual time, 10 000 frames at 120 fps a run, every
// packet through the real packetizer and reassembler.
//
// Frames have realistic shard counts, at the internet path's largest shard:
// a 1440p game's, and the still pattern's in HEVC. Which frames survive
// depends on the counts alone. Each count comes in four lengths, from the
// shortest to the longest it carries, so shards of many lengths are in play
// at once, as in a real stream.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use channels::video::{
    Event, FRAME_HEADER, FrameFacts, HEADER, Packetizer, Reassembler, VideoNumbers, parity_count,
    parity_for_loss, parity_percent,
};

const LARGEST: usize = 1152;
const FRAMES: u32 = 10_000;
const INTERVAL: Duration = Duration::from_nanos(8_333_333);
// One way, in each direction.
const DELAY: Duration = Duration::from_millis(5);
// How often the viewer tells the sharer its loss, as voice's reports do.
const REPORT_EVERY: Duration = Duration::from_secs(1);

struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u32) -> u32 {
        (self.next() % u64::from(n)) as u32
    }

    fn chance(&mut self, p: f64) -> bool {
        ((self.next() >> 11) as f64 / (1u64 << 53) as f64) < p
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Loss {
    Random(f64),
    // Runs of 3 to 10 packets, starting often enough for about this much.
    Bursts(f64),
}

#[derive(Clone, Copy, PartialEq)]
enum Parity {
    Fixed(u32),
    // The percentage for the reported loss, every frame alike.
    Percentage,
    // Each frame's own from the reported loss, by parity_for_loss.
    Rule,
}

#[derive(Clone, Copy, PartialEq)]
enum Mix {
    // A 1440p game: an IDR of 66 to 78 shards every 2 s, 2 to 20 shards
    // otherwise.
    Game,
    // The test pattern in NVENC's HEVC, 3 to 5 shards a frame, with the
    // same IDRs.
    StillHevc,
}

#[derive(Clone, Copy, PartialEq)]
enum Encoder {
    // NVENC: a lost frame is invalidated and the frames after it decode.
    Invalidates,
    // Everyone else: an IDR after a loss, and nothing decodes until it comes.
    NeedsIdr,
}

struct Run {
    numbers: VideoNumbers,
    sent_packets: u64,
    lost_packets: u64,
    data_shards: u64,
    parity_shards: u64,
    idrs: u32,
    // The longest time without a frame coming out.
    longest_freeze: Duration,
    // Packet lengths sent, each counted once.
    lengths: usize,
}

// Packets for each shape of frame, made once: two frames of one shape differ
// only in the frame number, which sits outside the parity.
struct Sharer {
    packetizer: Packetizer,
    made: HashMap<(u32, u16, bool, usize), Vec<Vec<u8>>>,
    survives: bool,
}

impl Sharer {
    // `reported` is the viewer's last loss report, if one came.
    fn packets(
        &mut self,
        number: u32,
        data: u32,
        parity: Parity,
        reported: Option<f32>,
        idr: bool,
    ) -> Vec<Vec<u8>> {
        let (packetizer, survives) = (&mut self.packetizer, self.survives);
        // From the frame number, so the loss and the counts draw the same
        // numbers as they did when every shard was the largest.
        let length = number as usize % 4;
        let shards = data as u16;
        let count = match parity {
            Parity::Fixed(percent) => parity_count(shards, percent),
            Parity::Percentage => parity_count(shards, parity_percent(reported)),
            Parity::Rule => parity_for_loss(shards, reported),
        };
        let key = (data, count, idr, length);
        let made = self.made.entry(key).or_insert_with(|| {
            let shortest = (data as usize - 1) * LARGEST - FRAME_HEADER + 1;
            let len = shortest + length * (LARGEST - 1) / 3;
            let unit: Vec<u8> = (0..len).map(|i| (i * 31 + data as usize) as u8).collect();
            let facts = FrameFacts {
                number: 0,
                idr,
                survives_loss: survives,
                hevc: false,
                captured: 1,
                encoded: 2,
            };
            let sent = match parity {
                Parity::Fixed(percent) => packetizer.packetize(&facts, &unit, percent),
                Parity::Percentage => packetizer.packetize(&facts, &unit, parity_percent(reported)),
                Parity::Rule => packetizer.packetize_for_loss(&facts, &unit, reported),
            }
            .unwrap();
            assert_eq!((sent.data(), sent.parity()), (shards, count));
            sent.iter().map(<[u8]>::to_vec).collect()
        });
        made.iter()
            .map(|packet| {
                let mut packet = packet.clone();
                packet[..4].copy_from_slice(&number.to_le_bytes());
                packet
            })
            .collect()
    }
}

struct Viewer {
    reassembler: Reassembler,
    encoder: Encoder,
    // Frame numbers sent as IDRs. Nothing before an IDR is kept to predict
    // from, so NVENC answers a lost one with another IDR too.
    idrs: Vec<u32>,
    // When the sharer hears a recover request it answers with an IDR.
    idr_due: Option<Instant>,
    last_out: Instant,
    longest_freeze: Duration,
}

impl Viewer {
    fn events(&mut self, at: Instant) {
        while let Some(event) = self.reassembler.event() {
            match event {
                Event::Frame(_) => {
                    self.longest_freeze = self.longest_freeze.max(at - self.last_out);
                    self.last_out = at;
                }
                Event::Recover { first, last }
                    if self.encoder == Encoder::NeedsIdr
                        || self.idrs.iter().any(|idr| (first..=last).contains(idr)) =>
                {
                    self.idr_due.get_or_insert(at + DELAY);
                }
                _ => {}
            }
        }
    }

    // Deadlines that fall before `at`, as the viewer's timer would fire them.
    fn wait_until(&mut self, at: Instant) {
        while let Some(deadline) = self.reassembler.next_deadline().filter(|&d| d <= at) {
            self.reassembler.expire(deadline);
            self.events(deadline);
        }
    }
}

fn run(mix: Mix, loss: Loss, parity: Parity, encoder: Encoder, seed: u64) -> Run {
    let mut random = Random(seed);
    let mut sharer = Sharer {
        packetizer: Packetizer::new(HEADER + LARGEST).unwrap(),
        made: HashMap::new(),
        survives: encoder == Encoder::Invalidates,
    };
    let start = Instant::now();
    let mut viewer = Viewer {
        reassembler: Reassembler::new(INTERVAL),
        encoder,
        idrs: Vec::new(),
        idr_due: None,
        last_out: start,
        longest_freeze: Duration::ZERO,
    };
    let mut reported = None;
    let mut next_report = start + REPORT_EVERY;
    let mut burst_left = 0;
    let (mut sent_packets, mut lost_packets, mut idrs) = (0, 0, 0);
    let (mut data_shards, mut parity_shards) = (0, 0);
    let mut lengths = HashSet::new();

    for number in 0..FRAMES {
        let sent_at = start + INTERVAL * number;
        let arrive = sent_at + DELAY;
        if sent_at >= next_report {
            // The report left the viewer DELAY ago.
            reported = viewer.reassembler.loss(sent_at - DELAY).percent();
            next_report += REPORT_EVERY;
        }
        let asked = viewer.idr_due.is_some_and(|due| due <= sent_at);
        if asked {
            viewer.idr_due = None;
        }
        // An IDR every 2 s stands for a viewer joining.
        let idr = number % 240 == 0 || asked;
        let data = if idr {
            idrs += 1;
            viewer.idrs.push(number);
            66 + random.below(13)
        } else if mix == Mix::StillHevc {
            3 + random.below(3)
        } else {
            2 + random.below(19)
        };
        let packets = sharer.packets(number, data, parity, reported, idr);
        data_shards += u64::from(data);
        parity_shards += packets.len() as u64 - u64::from(data);

        viewer.wait_until(arrive);
        for packet in &packets {
            sent_packets += 1;
            lengths.insert(packet.len());
            let lost = match loss {
                Loss::Random(p) => random.chance(p),
                Loss::Bursts(p) => {
                    if burst_left == 0 && random.chance(p / (6.5 - 5.5 * p)) {
                        burst_left = 3 + random.below(8);
                    }
                    let lost = burst_left > 0;
                    burst_left = burst_left.saturating_sub(1);
                    lost
                }
            };
            if lost {
                lost_packets += 1;
                continue;
            }
            viewer.reassembler.push(packet, arrive);
            viewer.events(arrive);
        }
    }
    let end = start + INTERVAL * FRAMES + Duration::from_secs(1);
    viewer.wait_until(end);
    Run {
        numbers: viewer.reassembler.numbers(),
        sent_packets,
        lost_packets,
        data_shards,
        parity_shards,
        idrs,
        longest_freeze: viewer.longest_freeze,
        lengths: lengths.len(),
    }
}

fn row(name: &str, run: &Run) -> String {
    let n = &run.numbers;
    format!(
        "{name:<42} {:>9} {:>8} {:>7} {:>7} {:>6.1}% {:>6.1}% {:>5} {:>7.1} ms",
        n.delivered,
        n.repaired,
        n.dropped(),
        n.skipped,
        run.lost_packets as f64 * 100.0 / run.sent_packets as f64,
        run.parity_shards as f64 * 100.0 / run.data_shards as f64,
        run.idrs,
        run.longest_freeze.as_secs_f64() * 1000.0,
    )
}

const HEADING: &str = "frames, loss, parity, encoder              delivered repaired dropped skipped    lost parity  IDRs  longest freeze";

fn measure(loss: Loss, parity: Parity, encoder: Encoder) -> Run {
    measure_mix(Mix::Game, loss, parity, encoder)
}

// One run per test, so the runs share the cores. Every frame is accounted
// for and nothing is a protocol error.
fn measure_mix(mix: Mix, loss: Loss, parity: Parity, encoder: Encoder) -> Run {
    let mix_name = match mix {
        Mix::Game => "game",
        Mix::StillHevc => "still HEVC",
    };
    let (name, seed) = match loss {
        Loss::Random(p) => (format!("{:.0}%", p * 100.0), 10 + (p * 100.0) as u64),
        Loss::Bursts(p) => (format!("bursts {:.0}%", p * 100.0), 20 + (p * 100.0) as u64),
    };
    let parity_name = match parity {
        Parity::Fixed(percent) => percent.to_string(),
        Parity::Percentage => String::from("percentage"),
        Parity::Rule => String::from("rule"),
    };
    let encoder_name = match encoder {
        Encoder::Invalidates => "invalidates",
        Encoder::NeedsIdr => "needs IDR",
    };
    let run = run(mix, loss, parity, encoder, seed);
    println!(
        "{HEADING}\n{}",
        row(
            &format!("{mix_name}, {name}, {parity_name}, {encoder_name}"),
            &run
        )
    );
    let n = &run.numbers;
    assert_eq!(n.delivered + n.dropped() + n.skipped, u64::from(FRAMES));
    assert_eq!(n.protocol_errors, 0);
    assert!(run.lengths > 5, "only {} packet lengths", run.lengths);
    run
}

use Encoder::{Invalidates, NeedsIdr};
use Parity::{Fixed, Percentage, Rule};

fn random(percent: u32) -> Loss {
    Loss::Random(f64::from(percent) / 100.0)
}

fn bursts(percent: u32) -> Loss {
    Loss::Bursts(f64::from(percent) / 100.0)
}

fn clean(run: Run) {
    let n = run.numbers;
    assert_eq!(
        (n.delivered, n.repaired, n.dropped()),
        (u64::from(FRAMES), 0, 0)
    );
    assert_eq!(run.longest_freeze, INTERVAL);
}

#[test]
fn no_loss_parity_20() {
    clean(measure(random(0), Fixed(20), Invalidates));
}

#[test]
fn no_loss_parity_rule() {
    clean(measure(random(0), Rule, Invalidates));
}

#[test]
fn loss_1_parity_20() {
    measure(random(1), Fixed(20), Invalidates);
}

#[test]
fn loss_1_parity_rule() {
    measure(random(1), Rule, Invalidates);
}

// Not none: a frame of up to five data shards carries one parity shard at
// 20 percent, and two of its six packets are lost 3 percent of the time at
// 5 percent loss.
#[test]
fn loss_5_parity_20() {
    let run = measure(random(5), Fixed(20), Invalidates);
    assert!(
        run.numbers.dropped() * 100 < u64::from(FRAMES) * 2,
        "{:?}",
        run.numbers
    );
}

#[test]
fn loss_5_parity_percentage() {
    measure(random(5), Percentage, Invalidates);
}

// Under 1 in 100 at 5 percent, each drop hidden by invalidation: no whole
// frame waits for an IDR, and no IDR is sent beyond the one every 2 s. Twice
// 5 percent is under the floor, and frames of 3 to 5 and 8 to 10 data shards
// get one parity shard more than it gives.
#[test]
fn loss_5_parity_rule() {
    let run = measure(random(5), Rule, Invalidates);
    assert!(
        run.numbers.dropped() * 100 < u64::from(FRAMES),
        "{:?}",
        run.numbers
    );
    assert_eq!(run.numbers.skipped, 0, "{:?}", run.numbers);
    assert_eq!(run.idrs, FRAMES.div_ceil(240));
}

#[test]
fn loss_5_parity_rule_needs_idr() {
    measure(random(5), Rule, NeedsIdr);
}

#[test]
fn loss_10_parity_20() {
    measure(random(10), Fixed(20), Invalidates);
}

#[test]
fn loss_10_parity_percentage() {
    measure(random(10), Percentage, Invalidates);
}

#[test]
fn loss_10_parity_rule() {
    measure(random(10), Rule, Invalidates);
}

// The still pattern in HEVC, frames of 3 to 5 data shards, with the
// percentage alone loses about 2 in 100 for good at 5 percent loss, and with
// the rule under 1 in 100.
#[test]
fn still_hevc_loss_5_parity_percentage() {
    let run = measure_mix(Mix::StillHevc, random(5), Percentage, Invalidates);
    assert!(
        run.numbers.dropped() * 100 > u64::from(FRAMES),
        "{:?}",
        run.numbers
    );
}

#[test]
fn still_hevc_loss_5_parity_rule() {
    let run = measure_mix(Mix::StillHevc, random(5), Rule, Invalidates);
    assert!(
        run.numbers.dropped() * 100 < u64::from(FRAMES),
        "{:?}",
        run.numbers
    );
    assert_eq!(run.numbers.skipped, 0, "{:?}", run.numbers);
}

#[test]
fn still_hevc_loss_10_parity_rule() {
    measure_mix(Mix::StillHevc, random(10), Rule, Invalidates);
}

#[test]
fn still_hevc_loss_20_parity_rule() {
    measure_mix(Mix::StillHevc, random(20), Rule, Invalidates);
}

#[test]
fn loss_20_parity_20() {
    measure(random(20), Fixed(20), Invalidates);
}

// With the parity following the loss the picture keeps moving: nine frames
// in ten come out and no freeze reaches a tenth of a second.
#[test]
fn loss_20_parity_rule() {
    let run = measure(random(20), Rule, Invalidates);
    assert!(
        run.numbers.delivered * 10 > u64::from(FRAMES) * 9,
        "{:?}",
        run.numbers
    );
    assert!(run.longest_freeze < Duration::from_millis(100));
}

// An encoder that needs an IDR recovers each time the parity is beaten.
#[test]
fn loss_20_parity_rule_needs_idr() {
    let run = measure(random(20), Rule, NeedsIdr);
    assert!(
        run.numbers.delivered * 10 > u64::from(FRAMES) * 7,
        "{:?}",
        run.numbers
    );
    assert!(run.longest_freeze < Duration::from_millis(500));
}

#[test]
fn loss_30_parity_20() {
    measure(random(30), Fixed(20), Invalidates);
}

#[test]
fn loss_30_parity_rule() {
    measure(random(30), Rule, Invalidates);
}

#[test]
fn bursts_1_parity_20() {
    measure(bursts(1), Fixed(20), Invalidates);
}

#[test]
fn bursts_1_parity_rule() {
    measure(bursts(1), Rule, Invalidates);
}

#[test]
fn bursts_5_parity_20() {
    measure(bursts(5), Fixed(20), Invalidates);
}

#[test]
fn bursts_5_parity_rule() {
    measure(bursts(5), Rule, Invalidates);
}

#[test]
fn bursts_10_parity_rule() {
    measure(bursts(10), Rule, Invalidates);
}
