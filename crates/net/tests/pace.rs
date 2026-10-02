use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use net::pace::{Burst, Pacer, SPREAD_MARGIN, Timer};

const FPS_120: Duration = Duration::from_nanos(8_333_333);

fn percentile(sorted: &[Duration], p: usize) -> Duration {
    sorted[(sorted.len() * p / 100).min(sorted.len() - 1)]
}

// How late the timer wakes past the time it was set for.
fn lateness(timer: &Timer, wait: Duration, rounds: usize) -> Vec<Duration> {
    let mut late = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let start = Instant::now();
        timer.set(wait).unwrap();
        timer.wait().unwrap();
        let took = start.elapsed();
        assert!(took >= wait, "woke {:?} early", wait - took);
        late.push(took - wait);
    }
    late.sort();
    late
}

#[test]
fn timer_wakes_close_to_the_time_set() {
    let timer = Timer::new().unwrap();
    assert!(timer.high_resolution(), "{:?}", timer.note());
    for wait in [Duration::from_micros(500), Duration::from_millis(2)] {
        let late = lateness(&timer, wait, 200);
        println!(
            "{wait:?} wait over 200: median {:?} late, p95 {:?}, p99 {:?}, worst {:?}",
            percentile(&late, 50),
            percentile(&late, 95),
            percentile(&late, 99),
            late[late.len() - 1]
        );
        assert!(percentile(&late, 50) < Duration::from_millis(2));
    }
}

// Each packet says which frame and which packet it is.
fn frame(pacer: &Pacer, number: u8, packets: u16, len: usize) -> Burst {
    let mut burst = pacer.burst();
    let mut packet = vec![0; len];
    for index in 0..packets {
        packet[0] = number;
        packet[1..3].copy_from_slice(&index.to_le_bytes());
        burst.push(&packet);
    }
    burst
}

struct Sent {
    at: Instant,
    frame: u8,
    index: u16,
    len: usize,
}

fn recording() -> (Pacer, Receiver<Sent>) {
    let (sender, receiver) = mpsc::channel();
    let pacer = Pacer::start(move |packet: &[u8]| {
        let _ = sender.send(Sent {
            at: Instant::now(),
            frame: packet[0],
            index: u16::from_le_bytes([packet[1], packet[2]]),
            len: packet.len(),
        });
    })
    .unwrap();
    assert!(pacer.note().is_none(), "{:?}", pacer.note());
    (pacer, receiver)
}

fn take(receiver: &Receiver<Sent>, count: usize) -> Vec<Sent> {
    (0..count)
        .map(|n| {
            receiver
                .recv_timeout(Duration::from_secs(5))
                .unwrap_or_else(|_| panic!("packet {n} of {count} never went out"))
        })
        .collect()
}

// When packet `nth` of `count` spread ones is due after the first.
fn due(nth: usize, count: usize, interval: Duration) -> Duration {
    let span = (interval / 2).saturating_sub(SPREAD_MARGIN);
    span * nth as u32 / count as u32
}

// Packets sent one after another inside a wake are microseconds apart at
// most; wakes are further apart than this.
const CLUMP_GAP: Duration = Duration::from_micros(20);

// The timer wakes under 1.7 ms late unless the whole PC stalls: in one run of
// these tests a 0.5 ms wait ended 7.7 ms late, and the frame being spread
// then went over half an interval with it. A gap longer than this between
// two packets of a frame is such a stall.
const HELD_UP: Duration = Duration::from_micros(1_900);

// The timer's granularity sets the clumps (pace.rs): each wake sends the
// packets that came due since the one before. At most two frames in 20 may
// be held up by a stall. In every other one the last packet goes inside half
// an interval (with no gap over 1.9 ms it goes a little over 4 ms after the
// first at 120 fps, at most), the tolerance against the even schedule is
// 1.5 ms at the 95th percentile, and the clumps must show a real spread: at
// least three wakes in each frame, and a typical clump no bigger than the
// packets due in a millisecond.
#[test]
fn spread_frame_timing() {
    let (pacer, receiver) = recording();
    let mut late = Vec::new();
    let mut lasts = Vec::new();
    let mut gaps = Vec::new();
    let mut clumps_per_frame = Vec::new();
    let mut clumps = Vec::new();
    let mut held_up = Vec::new();
    for number in 0..20u8 {
        pacer.put(frame(&pacer, number, 72, 1166), FPS_120, true);
        let sent = take(&receiver, 72);
        // The next frame starts at another point of the timer's tick.
        let pause = Duration::from_micros(2_100 + 370 * u64::from(number));
        let first = sent[0].at;
        for (nth, packet) in sent.iter().enumerate() {
            assert_eq!(
                (packet.frame, packet.index, packet.len),
                (number, nth as u16, 1166)
            );
            // A packet goes at or after its time; the first packet's own
            // time is a few microseconds after the thread took the frame.
            assert!(
                packet.at - first + Duration::from_micros(50) >= due(nth, 72, FPS_120),
                "packet {nth} went early"
            );
        }
        let last = sent[71].at - first;
        let longest = sent
            .windows(2)
            .map(|pair| pair[1].at - pair[0].at)
            .max()
            .unwrap_or_default();
        if longest > HELD_UP {
            held_up.push((number, longest, last));
            thread::sleep(pause);
            continue;
        }
        for (nth, packet) in sent.iter().enumerate() {
            late.push((packet.at - first).saturating_sub(due(nth, 72, FPS_120)));
        }
        let mut clump = 1;
        let mut count = 0;
        for pair in sent.windows(2) {
            let gap = pair[1].at - pair[0].at;
            gaps.push(gap);
            if gap > CLUMP_GAP {
                clumps.push(clump);
                count += 1;
                clump = 1;
            } else {
                clump += 1;
            }
        }
        clumps.push(clump);
        clumps_per_frame.push(count + 1);
        assert!(
            last < FPS_120 / 2,
            "frame {number}: last packet {last:?} after the first"
        );
        lasts.push(last);
        thread::sleep(pause);
    }
    // (frame, longest gap, last packet after the first)
    println!("frames held up by a stall: {held_up:?}");
    assert!(held_up.len() <= 2, "{held_up:?}");
    late.sort();
    lasts.sort();
    gaps.sort();
    clumps.sort();
    clumps_per_frame.sort();
    let wake_gaps: Vec<Duration> = gaps
        .iter()
        .copied()
        .filter(|&gap| gap > CLUMP_GAP)
        .collect();
    println!(
        "72 packets at 120 fps, {} frames not held up: last packet after the first median {:?}, worst {:?} \
         (limit {:?}); lateness against the even schedule median {:?}, p95 {:?}, worst {:?}; \
         clumps per frame fewest {}, median {}; packets per clump median {}, largest {}; \
         gaps between clumps median {:?}, worst {:?}",
        lasts.len(),
        percentile(&lasts, 50),
        lasts[lasts.len() - 1],
        FPS_120 / 2,
        percentile(&late, 50),
        percentile(&late, 95),
        late[late.len() - 1],
        clumps_per_frame[0],
        clumps_per_frame[clumps_per_frame.len() / 2],
        clumps[clumps.len() / 2],
        clumps[clumps.len() - 1],
        percentile(&wake_gaps, 50),
        wake_gaps[wake_gaps.len() - 1],
    );
    assert!(percentile(&late, 95) < Duration::from_micros(1_500));
    assert!(clumps_per_frame[0] >= 3, "{clumps_per_frame:?}");
    let span = (FPS_120 / 2).saturating_sub(SPREAD_MARGIN);
    let due_in_a_millisecond = (72 * 1_000 / span.as_micros()) as usize;
    assert!(
        clumps[clumps.len() / 2] <= due_in_a_millisecond,
        "median clump {} packets; {due_in_a_millisecond} are due in a millisecond",
        clumps[clumps.len() / 2]
    );
    assert_eq!(pacer.numbers().frames, 20);
    assert_eq!(pacer.numbers().packets, 20 * 72);
    assert_eq!(pacer.numbers().cut_short, 0);
}

// The old frame is spread over 23 ms, so the new one comes in the middle of
// it even when this thread is kept waiting for a few milliseconds.
#[test]
fn new_frame_flushes_the_old_one() {
    let (pacer, receiver) = recording();
    pacer.put(frame(&pacer, 1, 72, 1166), Duration::from_millis(50), true);
    let early = take(&receiver, 10);
    let put_at = Instant::now();
    pacer.put(frame(&pacer, 2, 72, 1166), FPS_120, true);
    let rest = take(&receiver, 62 + 72);
    let (old, new) = rest.split_at(62);
    // The old frame's packets all went, in order, before the new frame's
    // first one, and within a moment of the new frame arriving.
    for (nth, packet) in early.iter().chain(old).enumerate() {
        assert_eq!((packet.frame, packet.index), (1, nth as u16));
    }
    let flushed = old[61].at.saturating_duration_since(put_at);
    assert!(
        flushed < Duration::from_millis(2),
        "the rest took {flushed:?}"
    );
    assert!(new[0].at >= old[61].at);
    for (nth, packet) in new.iter().enumerate() {
        assert_eq!((packet.frame, packet.index), (2, nth as u16));
    }
    // And the new frame is spread as usual. Its first packet went after the
    // flush, so its last is up to that much less than the spread after it.
    let last = new[71].at - new[0].at;
    assert!(
        last > Duration::from_millis(1) && last < FPS_120 / 2,
        "{last:?}"
    );
    println!("62 packets left of the old frame went {flushed:?} after the new one came");
    let numbers = pacer.numbers();
    assert_eq!(
        (numbers.frames, numbers.packets, numbers.cut_short),
        (2, 144, 1)
    );
}

#[test]
fn without_spread_every_packet_goes_at_once() {
    let (pacer, receiver) = recording();
    for number in 0..10u8 {
        pacer.put(frame(&pacer, number, 72, 1166), FPS_120, false);
        let sent = take(&receiver, 72);
        let took = sent[71].at - sent[0].at;
        assert!(took < Duration::from_millis(1), "72 packets took {took:?}");
    }
    assert_eq!(pacer.numbers().packets, 720);
}

#[test]
fn one_packet_and_empty_frames() {
    let (pacer, receiver) = recording();
    pacer.put(frame(&pacer, 1, 1, 40), FPS_120, true);
    let sent = take(&receiver, 1);
    assert_eq!((sent[0].frame, sent[0].len), (1, 40));
    pacer.put(pacer.burst(), FPS_120, true);
    pacer.put(frame(&pacer, 2, 3, 40), FPS_120, true);
    let sent = take(&receiver, 3);
    assert!(sent.iter().all(|packet| packet.frame == 2));
    assert!(receiver.recv_timeout(Duration::from_millis(20)).is_err());
}

// A frame interval that is not a video rate is not spread over half of it.
#[test]
fn long_interval_spread_cap() {
    let (pacer, receiver) = recording();
    pacer.put(frame(&pacer, 1, 10, 100), Duration::from_secs(1), true);
    let sent = take(&receiver, 10);
    let took = sent[9].at - sent[0].at;
    assert!(took < Duration::from_millis(25), "{took:?}");
}

// Each frame is spread over 23 ms, so a stop comes in the middle of it even
// when this thread is kept waiting for a few milliseconds.
#[test]
fn stop_is_prompt_and_joins_the_thread() {
    let mut worst = Duration::ZERO;
    for _ in 0..20 {
        let (pacer, receiver) = recording();
        pacer.put(frame(&pacer, 1, 72, 1166), Duration::from_millis(50), true);
        take(&receiver, 5);
        let start = Instant::now();
        drop(pacer);
        worst = worst.max(start.elapsed());
        // The thread is gone with the sender it owned, and whatever of the
        // frame was still waiting was not sent.
        let left: Vec<Sent> = receiver.try_iter().collect();
        assert!(left.len() < 67);
        assert!(receiver.recv().is_err());
    }
    println!("stopping mid-frame took at most {worst:?} over 20 stops");
    assert!(worst < Duration::from_millis(20), "{worst:?}");
}

// Held up inside the send function, as by a send that blocks on a full
// socket buffer, the thread cannot take frames as they come. The ones it
// never started are dropped, the rest of the one it was sending goes at
// once, and the newest is spread as usual.
#[test]
fn unstarted_frames_are_dropped() {
    let (gate, opened) = mpsc::channel::<()>();
    let (reached, held) = mpsc::channel::<()>();
    let (sender, receiver) = mpsc::channel();
    let pacer = Pacer::start(move |packet: &[u8]| {
        if packet[0] == 1 && packet[1] == 0 {
            let _ = reached.send(());
            let _ = opened.recv();
        }
        let _ = sender.send((Instant::now(), packet[0], packet[1]));
    })
    .unwrap();
    let burst = |number: u8, count: u8| {
        let mut burst = pacer.burst();
        for index in 0..count {
            burst.push(&[number, index]);
        }
        burst
    };
    pacer.put(burst(1, 4), FPS_120, true);
    held.recv_timeout(Duration::from_secs(5))
        .expect("the thread never reached the send function");
    for number in 2..=5 {
        pacer.put(burst(number, 30), FPS_120, true);
    }
    gate.send(()).unwrap();
    let sent: Vec<(Instant, u8, u8)> = (0..34)
        .map(|_| receiver.recv_timeout(Duration::from_secs(5)).unwrap())
        .collect();
    let order: Vec<(u8, u8)> = sent
        .iter()
        .map(|&(_, frame, index)| (frame, index))
        .collect();
    let expected: Vec<(u8, u8)> = (0..4)
        .map(|index| (1, index))
        .chain((0..30).map(|index| (5, index)))
        .collect();
    assert_eq!(order, expected);
    let rest = sent[3].0 - sent[0].0;
    assert!(rest < Duration::from_millis(1), "{rest:?}");
    let spread = sent[33].0 - sent[4].0;
    assert!(spread > Duration::from_millis(1), "{spread:?}");
    assert!(receiver.recv_timeout(Duration::from_millis(20)).is_err());
    let numbers = pacer.numbers();
    assert_eq!(
        (
            numbers.frames,
            numbers.packets,
            numbers.cut_short,
            numbers.discarded
        ),
        (2, 34, 1, 3)
    );
}
