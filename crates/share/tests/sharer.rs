// The sharer's side on capture's test pattern: frames leave as packets
// through the send function with the facts a viewer needs, and the calls a
// room makes while a share runs (an IDR and smaller packets for someone who
// starts watching over a tunnel, a recover request, a loss report, a new
// bitrate, the step down to 1080p60 and back up, a new frame rate) do what
// they say, and reports about frames not sent yet do nothing. A GPU encoder
// made to stall or fail hands the share to the software encoder, and a share
// on Intel graphics starts there. Nothing of the screen is captured and no
// window opens; everything on the GPU happens on the test's own thread, one
// test at a time.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use capture::CursorUpdate;
use channels::video::{
    Event, FRAME_HEADER, FrameFacts, HEADER, PARITY_FLOOR, Reassembler, SHARD_STEP, parity_count,
    parity_for_loss, parity_percent,
};
use encode::{Codec, Kind, Settings};
use share::{
    Audience, Back, Choice, Clock, Line, PAYLOAD_INTERNET, PAYLOAD_LAN, SOFTWARE_DID_NOT_START,
    SOFTWARE_FAILED_TOO, SWITCH_GAP, Sent, Setup, Sharer, Software,
};

const NVIDIA: u32 = 0x10de;
const INTEL: u32 = 0x8086;

#[derive(Default)]
struct Watching {
    back: Vec<Back>,
    started: bool,
    takes_hevc: bool,
    sent: Vec<Sent>,
    lines: Vec<Line>,
}

impl Audience for Watching {
    fn cursor(&mut self, _: CursorUpdate) {}

    fn back(&mut self, into: &mut Vec<Back>) {
        into.append(&mut self.back);
    }

    fn started_watching(&mut self) -> bool {
        std::mem::take(&mut self.started)
    }

    fn takes_hevc(&mut self) -> bool {
        self.takes_hevc
    }

    fn sent(&mut self, frame: &Sent) {
        self.sent.push(*frame);
    }

    fn line(&mut self, line: Line) {
        self.lines.push(line);
    }
}

type Packets = Arc<Mutex<Vec<Vec<u8>>>>;

fn taken(packets: &Packets) -> Vec<Vec<u8>> {
    std::mem::take(&mut *packets.lock().unwrap_or_else(PoisonError::into_inner))
}

// The pacer sends on its own thread, a moment after the frame is put.
fn wait_for(packets: &Packets, count: usize) -> Vec<Vec<u8>> {
    let until = Instant::now() + Duration::from_secs(2);
    while packets.lock().unwrap_or_else(PoisonError::into_inner).len() < count
        && Instant::now() < until
    {
        std::thread::sleep(Duration::from_millis(1));
    }
    taken(packets)
}

// Every frame the packets make, whole, in order.
fn frames(packets: &[Vec<u8>]) -> Vec<FrameFacts> {
    let mut reassembler = Reassembler::new(Duration::from_secs(1) / 120);
    let mut out = Vec::new();
    let now = Instant::now();
    for packet in packets {
        reassembler.push(packet, now);
        while let Some(event) = reassembler.event() {
            match event {
                Event::Frame(frame) => out.push(frame.facts),
                other => panic!("{other:?} with no packet lost"),
            }
        }
    }
    out
}

// One share at a time on the GPU.
static TURN: Mutex<()> = Mutex::new(());

fn gpu() -> Option<(MutexGuard<'static, ()>, capture::Adapter)> {
    let turn = TURN.lock().unwrap_or_else(PoisonError::into_inner);
    let adapters = capture::adapters().unwrap_or_else(|err| panic!("{err}"));
    let Some(adapter) = adapters
        .iter()
        .find(|adapter| adapter.vendor_id == NVIDIA)
        .or(adapters.first())
    else {
        println!("skipped: this PC has no graphics card to make the pattern on");
        return None;
    };
    Some((turn, adapter.clone()))
}

#[test]
fn a_pattern_share_answers_the_room() {
    let Some((_turn, adapter)) = gpu() else {
        return;
    };
    let packets: Packets = Arc::default();
    let clock = Clock::starting(Instant::now());
    let setup = Setup {
        choice: Choice::Pattern {
            adapter,
            width: 2560,
            height: 1440,
            busy: false,
        },
        fps: 120,
        settings: Settings::default(),
        encoder: None,
        codec: Some(Codec::H264),
        takes_hevc: false,
        payload: PAYLOAD_LAN,
        spread: false,
        clock,
        // As in a room.
        keep_times: false,
    };
    let mut said = Vec::new();
    let send = {
        let packets = Arc::clone(&packets);
        move |packet: &[u8]| {
            packets
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(packet.to_vec());
        }
    };
    let mut sharer = Sharer::open(setup, send, &mut |line| said.push(line))
        .unwrap_or_else(|err| panic!("{err}"));
    let nvenc = sharer.encoder_name().starts_with("NVENC");
    println!(
        "{} at {:?} and {} fps; said {said:?}",
        sharer.encoder_name(),
        sharer.size(),
        sharer.fps()
    );
    assert_eq!((sharer.size(), sharer.fps()), ((2560, 1440), 120));

    let mut watching = Watching::default();
    for _ in 0..6 {
        sharer
            .next(&mut watching)
            .unwrap_or_else(|err| panic!("{err}"));
    }
    let sent = std::mem::take(&mut watching.sent);
    let numbers: Vec<u32> = sent.iter().map(|frame| frame.number).collect();
    assert_eq!(numbers, [0, 1, 2, 3, 4, 5]);
    assert!(sent[0].idr && sent[1..].iter().all(|frame| !frame.idr));
    let count = sent.iter().map(|frame| frame.packets).sum();
    let lan = wait_for(&packets, count);
    let facts = frames(&lan);
    assert_eq!(facts.len(), 6);
    let now = clock.micros(Instant::now());
    for (fact, frame) in facts.iter().zip(&sent) {
        assert_eq!((fact.number, fact.idr), (frame.number, frame.idr));
        // The flag says what the encoder does, on IDRs too.
        assert_eq!(fact.survives_loss, nvenc, "frame {}", fact.number);
        assert!(fact.captured <= fact.encoded && fact.encoded <= now);
    }
    // The IDR fills whole packets, bigger than a tunnel takes.
    let largest = lan.iter().map(Vec::len).max().unwrap_or(0);
    assert!(
        largest > PAYLOAD_INTERNET && largest <= PAYLOAD_LAN,
        "{largest}"
    );

    // Someone on a tunnel starts watching: smaller packets from the next
    // frame on, the IDR for them among the first, and the same viewer's
    // reassembler follows.
    assert!(sharer.set_payload(8).is_err(), "no room for a shard");
    sharer
        .set_payload(PAYLOAD_INTERNET)
        .unwrap_or_else(|err| panic!("{err}"));
    watching.started = true;
    sharer
        .next(&mut watching)
        .unwrap_or_else(|err| panic!("{err}"));
    let idr = wait_for(&packets, watching.sent[0].packets);
    assert!(idr.iter().all(|packet| packet.len() <= PAYLOAD_INTERNET));
    let facts = frames(&[lan, idr].concat());
    assert_eq!(facts.len(), 7);
    assert!(facts[6].idr, "an IDR for the new viewer");

    // A loss report and a lost frame, two reports about frames not sent
    // yet, which change nothing, and an ask for an IDR after the new
    // viewer's, which waits until that one has used a quarter of the
    // setting. At 1 Mbit/s an IDR of 1440p, tens of kilobytes, keeps that
    // floor over half a second, past the next frame even on a GPU busy with
    // a game.
    sharer
        .set_bitrate(1_000_000)
        .unwrap_or_else(|err| panic!("{err}"));
    watching.back = vec![
        Back::Loss(Some(15.0)),
        Back::Recover { first: 4, last: 4 },
        Back::Recover {
            first: 5000,
            last: 5000,
        },
        Back::Idr { seen: 5000 },
        Back::Idr { seen: 6 },
    ];
    sharer
        .next(&mut watching)
        .unwrap_or_else(|err| panic!("{err}"));
    let sent = std::mem::take(&mut watching.sent);
    assert!(
        sent[0].bytes > 20_000,
        "the floor after an IDR of {} bytes can end before the next frame",
        sent[0].bytes
    );
    assert!(sent[0].idr && !sent[1].idr);
    assert_eq!(sharer.parity(), parity_percent(Some(15.0)));
    assert_eq!(sent[1].parity, sharer.parity());
    let recovered = sharer.numbers();
    assert_eq!(recovered.recoveries, 1);
    assert_eq!(
        (recovered.invalidated, recovered.covered),
        (0, 1),
        "the IDR sent after frame 4 covers it"
    );
    assert_eq!(
        (recovered.unsent, recovered.idr_asks, recovered.floor_waits),
        (2, 1, 1)
    );
    sharer
        .set_bitrate(8_000_000)
        .unwrap_or_else(|err| panic!("{err}"));

    // The room's bitrate rule steps the share down.
    let mut said = Vec::new();
    let stepped = sharer.step_down(&mut |line| said.push(line));
    assert_eq!(stepped, Ok(true), "{said:?}");
    assert_eq!((sharer.size(), sharer.fps()), ((1920, 1080), 60));
    sharer
        .next(&mut watching)
        .unwrap_or_else(|err| panic!("{err}"));
    let first = watching.sent[0];
    assert_eq!((first.number, first.idr), (8, true));
    assert_eq!(sharer.step_down(&mut |_| {}), Ok(false));
    assert!(sharer.stepped_down());
    // The encoder opened again takes the rate asked for last.
    assert_eq!(sharer.encoder_bitrate(), 8_000_000);

    // A frame rate asked for while stepped down counts only under 1080p60's,
    // and stepping back up brings the size and the rest of the rate back.
    assert_eq!(sharer.set_fps(90, &mut |_| {}), Ok(false));
    assert_eq!(sharer.set_fps(30, &mut |_| {}), Ok(true));
    assert_eq!((sharer.size(), sharer.fps()), ((1920, 1080), 30));
    assert_eq!(sharer.step_up(&mut |_| {}), Ok(true));
    assert_eq!((sharer.size(), sharer.fps()), ((2560, 1440), 30));
    assert!(!sharer.stepped_down());
    assert_eq!(sharer.step_up(&mut |_| {}), Ok(false));
    assert_eq!(sharer.set_fps(120, &mut |_| {}), Ok(true));
    assert_eq!((sharer.size(), sharer.fps()), ((2560, 1440), 120));
    sharer
        .next(&mut watching)
        .unwrap_or_else(|err| panic!("{err}"));
    let up = *watching.sent.last().expect("a frame after the step up");
    assert_eq!((up.number, up.idr), (9, true), "frame numbers go on");

    let expected = watching.sent.len() + 8;
    let encode_ms: Vec<f32> = watching.sent.iter().map(|frame| frame.encode_ms).collect();
    let numbers = sharer.finish();
    assert_eq!(numbers.encoded as usize, expected);
    assert_eq!(numbers.idrs, 4);
    assert!(
        numbers.encode_ms.is_empty(),
        "a room keeps no list of times"
    );
    println!(
        "encoded {} frames, {} packets, {:.0} KB; encode ms after the step down {encode_ms:?}",
        numbers.encoded,
        numbers.pace.packets,
        numbers.bytes as f64 / 1000.0,
    );
    assert!(watching.lines.is_empty(), "{:?}", watching.lines);
}

// A 5120x1440 monitor, as the pattern stands in for one. H.264 on NVENC
// takes at most 4096 wide, so the share opens at 4096x1152, the step down
// for a slow link keeps that shape, and the step back up and a new frame
// rate come back to it rather than to the monitor's own width.
#[test]
fn a_super_wide_monitor_shares_at_4096_wide() {
    let Some((_turn, adapter)) = gpu() else {
        return;
    };
    let setup = Setup {
        choice: Choice::Pattern {
            adapter,
            width: 5120,
            height: 1440,
            busy: false,
        },
        fps: 120,
        settings: Settings::default(),
        encoder: None,
        codec: Some(Codec::H264),
        takes_hevc: false,
        payload: PAYLOAD_LAN,
        spread: false,
        clock: Clock::starting(Instant::now()),
        keep_times: false,
    };
    let mut said = Vec::new();
    let mut sharer = Sharer::open(setup, |_: &[u8]| {}, &mut |line| said.push(line))
        .unwrap_or_else(|err| panic!("{err}"));
    println!(
        "{} at {:?} and {} fps; said {said:?}",
        sharer.encoder_name(),
        sharer.size(),
        sharer.fps()
    );
    assert_eq!(sharer.size(), (4096, 1152));
    let mut watching = Watching::default();
    for _ in 0..3 {
        sharer
            .next(&mut watching)
            .unwrap_or_else(|err| panic!("{err}"));
    }
    assert_eq!(watching.sent.len(), 3);
    assert!(watching.sent[0].idr);

    // 1080p60's macroblocks at 32:9, as encode::software_fit works it out.
    assert_eq!(sharer.step_down(&mut |_| {}), Ok(true));
    assert_eq!((sharer.size(), sharer.fps()), ((2716, 764), 60));
    assert_eq!(sharer.set_fps(30, &mut |_| {}), Ok(true));
    assert_eq!((sharer.size(), sharer.fps()), ((2716, 764), 30));
    assert_eq!(sharer.step_up(&mut |_| {}), Ok(true));
    assert_eq!((sharer.size(), sharer.fps()), ((4096, 1152), 30));
    assert_eq!(sharer.set_fps(120, &mut |_| {}), Ok(true));
    assert_eq!((sharer.size(), sharer.fps()), ((4096, 1152), 120));
    sharer
        .next(&mut watching)
        .unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(watching.sent.len(), 4);
    assert!(watching.lines.is_empty(), "{:?}", watching.lines);
    let numbers = sharer.finish();
    assert_eq!(numbers.encoded, 4);
}

// A 1508x1425 monitor steps down to 1472x1390: the fit is 1472x1392, worked
// out from the share's 1508x1426, and capture, scaling the monitor's own
// 1425 lines to 1472 wide, rounds its height to 1390. Given 1472x1390 as
// limits, capture would fit the height instead and come out 1470 wide: a
// new size for nothing but a new frame rate. Most monitor sizes round alike
// both ways; this one does not.
#[test]
fn stepped_down_size_survives_a_new_fps() {
    let Some((_turn, adapter)) = gpu() else {
        return;
    };
    let setup = Setup {
        choice: Choice::Pattern {
            adapter,
            width: 1508,
            height: 1425,
            busy: false,
        },
        fps: 120,
        settings: Settings::default(),
        encoder: None,
        codec: Some(Codec::H264),
        takes_hevc: false,
        payload: PAYLOAD_LAN,
        spread: false,
        clock: Clock::starting(Instant::now()),
        keep_times: false,
    };
    let mut sharer =
        Sharer::open(setup, |_: &[u8]| {}, &mut |_| {}).unwrap_or_else(|err| panic!("{err}"));
    println!("{} at {:?}", sharer.encoder_name(), sharer.size());
    assert_eq!(sharer.size(), (1508, 1426));
    assert_eq!(sharer.step_down(&mut |_| {}), Ok(true));
    assert_eq!((sharer.size(), sharer.fps()), ((1472, 1390), 60));
    assert_eq!(sharer.set_fps(30, &mut |_| {}), Ok(true));
    assert_eq!((sharer.size(), sharer.fps()), ((1472, 1390), 30));
    assert_eq!(sharer.set_fps(60, &mut |_| {}), Ok(true));
    assert_eq!((sharer.size(), sharer.fps()), ((1472, 1390), 60));
    assert_eq!(sharer.step_up(&mut |_| {}), Ok(true));
    assert_eq!((sharer.size(), sharer.fps()), ((1508, 1426), 60));
    let mut watching = Watching::default();
    sharer
        .next(&mut watching)
        .unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(watching.sent.len(), 1);
    assert!(watching.lines.is_empty(), "{:?}", watching.lines);
}

// The next frame that goes out, with the codec it went in. The pattern
// makes one every frame interval.
fn next_frame(sharer: &mut Sharer, watching: &mut Watching) -> (Sent, Codec) {
    let before = watching.sent.len();
    for _ in 0..20 {
        sharer.next(watching).unwrap_or_else(|err| panic!("{err}"));
        if let Some(frame) = watching.sent.get(before) {
            let codec = sharer.codec().expect("an encoder");
            return (*frame, codec);
        }
    }
    panic!("no frame went out in 20 tries");
}

// Each frame takes its parity from the loss the viewers report, not a
// percentage alone. The busy pattern at 5 Mbit/s and 120 fps sends frames of
// about 5 KB, mostly four or five data shards, to which the 20 percent floor
// gives one parity shard and the rule two at 5 percent.
#[test]
fn a_shares_frames_take_their_parity_from_the_reported_loss() {
    let Some((_turn, adapter)) = gpu() else {
        return;
    };
    let setup = Setup {
        choice: Choice::Pattern {
            adapter,
            width: 640,
            height: 360,
            busy: true,
        },
        fps: 120,
        settings: Settings {
            bitrate: 5_000_000,
            ..Settings::default()
        },
        encoder: None,
        codec: Some(Codec::H264),
        takes_hevc: false,
        payload: PAYLOAD_INTERNET,
        spread: false,
        clock: Clock::starting(Instant::now()),
        keep_times: false,
    };
    let mut sharer =
        Sharer::open(setup, |_: &[u8]| {}, &mut |_| {}).unwrap_or_else(|err| panic!("{err}"));
    let mut watching = Watching {
        back: vec![Back::Loss(Some(5.0))],
        ..Watching::default()
    };
    for _ in 0..30 {
        sharer
            .next(&mut watching)
            .unwrap_or_else(|err| panic!("{err}"));
    }
    let shard = (PAYLOAD_INTERNET - HEADER) / SHARD_STEP * SHARD_STEP;
    let mut shards = Vec::new();
    for frame in &watching.sent {
        let data = (FRAME_HEADER + frame.bytes).div_ceil(shard) as u16;
        let parity = parity_for_loss(data, Some(5.0));
        assert_eq!(
            frame.packets,
            usize::from(data + parity),
            "frame {} of {data} data shards",
            frame.number
        );
        shards.push((data, parity));
    }
    println!("data and parity shards of each frame: {shards:?}");
    assert!(
        shards
            .iter()
            .any(|&(data, parity)| parity > parity_count(data, PARITY_FLOOR)),
        "no frame the floor rounds badly for"
    );
    assert_eq!(sharer.finish().encoded, 30);
}

// A share left to pick its codec, as a room's is: HEVC while every viewer
// takes it, H.264 from the next frame once one does not, that frame its
// one IDR, also for the new watcher who needs it, and HEVC again only a
// whole SWITCH_GAP after the last frame that needed H.264, again with one
// IDR. Frame numbers go on through both changes, and every frame's header
// says its codec.
#[test]
fn the_codec_follows_the_viewers() {
    let Some((_turn, adapter)) = gpu() else {
        return;
    };
    let packets: Packets = Arc::default();
    let setup = Setup {
        choice: Choice::Pattern {
            adapter,
            width: 2560,
            height: 1440,
            busy: false,
        },
        fps: 120,
        settings: Settings::default(),
        encoder: None,
        codec: None,
        takes_hevc: true,
        payload: PAYLOAD_LAN,
        spread: false,
        clock: Clock::starting(Instant::now()),
        keep_times: false,
    };
    let send = {
        let packets = Arc::clone(&packets);
        move |packet: &[u8]| {
            packets
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(packet.to_vec());
        }
    };
    let mut said = Vec::new();
    let mut sharer = Sharer::open(setup, send, &mut |line| said.push(line))
        .unwrap_or_else(|err| panic!("{err}"));
    println!(
        "{} at {:?}; said {said:?}",
        sharer.encoder_name(),
        sharer.size()
    );
    if sharer.codec() != Some(Codec::Hevc) {
        println!("skipped: no encoder here takes HEVC at 2560x1440 and 120 fps");
        return;
    }
    let mut watching = Watching {
        takes_hevc: true,
        ..Watching::default()
    };
    let mut sent = Vec::new();
    for _ in 0..5 {
        sent.push(next_frame(&mut sharer, &mut watching));
    }
    // Someone who does not take HEVC starts watching: the IDR they need is
    // the new encoder's first frame.
    watching.takes_hevc = false;
    watching.started = true;
    for _ in 0..4 {
        sent.push(next_frame(&mut sharer, &mut watching));
    }
    // Before the last frame for them: it needs H.264 from a moment after.
    let last_needed = Instant::now();
    sent.push(next_frame(&mut sharer, &mut watching));
    // They stop.
    watching.takes_hevc = true;
    let back = loop {
        let (frame, codec) = next_frame(&mut sharer, &mut watching);
        sent.push((frame, codec));
        if codec == Codec::Hevc {
            break last_needed.elapsed();
        }
        assert!(
            last_needed.elapsed() < SWITCH_GAP * 2,
            "still {codec} {:?} after the last frame that needed H.264",
            last_needed.elapsed()
        );
    };
    for _ in 0..3 {
        sent.push(next_frame(&mut sharer, &mut watching));
    }
    let codecs: Vec<Codec> = sent.iter().map(|(_, codec)| *codec).collect();
    let idrs: Vec<u32> = sent
        .iter()
        .filter(|(frame, _)| frame.idr)
        .map(|(frame, _)| frame.number)
        .collect();
    let back_at = 10
        + codecs[10..]
            .iter()
            .position(|c| *c == Codec::Hevc)
            .unwrap_or(0);
    println!(
        "{} frames: HEVC to H.264 at frame 5, back to HEVC at frame {back_at}, {back:?} after the last frame that needed H.264; IDRs {idrs:?}; the encoder now {}",
        sent.len(),
        sharer.encoder_name()
    );
    assert!(codecs[..5].iter().all(|c| *c == Codec::Hevc));
    assert!(codecs[5..back_at].iter().all(|c| *c == Codec::H264));
    assert!(codecs[back_at..].iter().all(|c| *c == Codec::Hevc));
    assert_eq!(idrs, [0, 5, back_at as u32], "one IDR for each change");
    assert!(back >= SWITCH_GAP, "HEVC came back after {back:?}");
    let numbers: Vec<u32> = sent.iter().map(|(frame, _)| frame.number).collect();
    assert_eq!(numbers, (0..sent.len() as u32).collect::<Vec<_>>());
    assert_eq!(sharer.numbers().codec_changes, 2);

    // What the viewer reads from the headers.
    let count = sent.iter().map(|(frame, _)| frame.packets).sum();
    let facts = frames(&wait_for(&packets, count));
    assert_eq!(facts.len(), sent.len());
    for (fact, (frame, codec)) in facts.iter().zip(&sent) {
        assert_eq!(
            (fact.number, fact.idr, fact.hevc),
            (frame.number, frame.idr, *codec == Codec::Hevc)
        );
    }
    let lines: Vec<&Line> = watching.lines.iter().collect();
    println!("said {lines:?}");
}

// A share of the pattern at `size` and `fps` that sends into `packets`,
// with what it said while it opened. Every viewer takes HEVC.
fn share_on(
    adapter: capture::Adapter,
    packets: &Packets,
    (width, height): (u32, u32),
    fps: u32,
    encoder: Option<Kind>,
    codec: Option<Codec>,
) -> Result<(Sharer, Vec<Line>), String> {
    let setup = Setup {
        choice: Choice::Pattern {
            adapter,
            width,
            height,
            busy: false,
        },
        fps,
        settings: Settings::default(),
        encoder,
        codec,
        takes_hevc: true,
        payload: PAYLOAD_LAN,
        spread: false,
        clock: Clock::starting(Instant::now()),
        keep_times: false,
    };
    let send = {
        let packets = Arc::clone(packets);
        move |packet: &[u8]| {
            packets
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(packet.to_vec());
        }
    };
    let mut said = Vec::new();
    let sharer = Sharer::open(setup, send, &mut |line| said.push(line))?;
    println!(
        "{} at {:?} and {} fps; said {said:?}",
        sharer.encoder_name(),
        sharer.size(),
        sharer.fps()
    );
    Ok((sharer, said))
}

// A share on the Media Foundation hardware encoder, as on an AMD GPU or an
// Intel one when a test asks for it, in `codec` or left to pick (which
// picks HEVC here). Pinned to NVIDIA's Media Foundation encoder here, which
// shares no code with NVENC.
fn hardware_share(
    adapter: capture::Adapter,
    packets: &Packets,
    size: (u32, u32),
    fps: u32,
    codec: Option<Codec>,
) -> Option<Sharer> {
    let (sharer, _) = match share_on(adapter, packets, size, fps, Some(Kind::MfHardware), codec) {
        Ok(opened) => opened,
        Err(err) => {
            println!("skipped: no Media Foundation hardware encoder here: {err}");
            return None;
        }
    };
    let wanted = codec.unwrap_or(Codec::Hevc);
    if sharer.codec() != Some(wanted) {
        println!(
            "skipped: the Media Foundation hardware encoder here takes no {wanted} at {size:?}"
        );
        return None;
    }
    Some(sharer)
}

fn texts(lines: &[Line]) -> Vec<&str> {
    lines
        .iter()
        .map(|line| match line {
            Line::Say(text) | Line::Log(text) => text.as_str(),
        })
        .collect()
}

// The log line for the software encoder's first frame after a GPU encoder
// failed, and the milliseconds it gives from the failure to that frame.
fn first_software_frame(lines: &[Line]) -> (String, f32) {
    let line = texts(lines)
        .into_iter()
        .find(|line| line.contains(" to Media Foundation H.264, software, 1080p60 in "))
        .unwrap_or_else(|| panic!("no line for the software encoder's first frame: {lines:#?}"))
        .to_string();
    let took: f32 = line
        .split(" 1080p60 in ")
        .nth(1)
        .and_then(|rest| rest.split(" ms").next())
        .and_then(|ms| ms.parse().ok())
        .unwrap_or_else(|| panic!("{line}"));
    (line, took)
}

fn numbers_and_idrs(sent: &[(Sent, Codec)]) -> (Vec<u32>, Vec<u32>) {
    let numbers = sent.iter().map(|(frame, _)| frame.number).collect();
    let idrs = sent
        .iter()
        .filter(|(frame, _)| frame.idr)
        .map(|(frame, _)| frame.number)
        .collect();
    (numbers, idrs)
}

// What the viewer reads from the headers: each frame whole, in order, with
// its codec.
fn check_headers(packets: &Packets, watching: &Watching, sent: &[(Sent, Codec)]) {
    let count = watching.sent.iter().map(|frame| frame.packets).sum();
    let facts = frames(&wait_for(packets, count));
    assert_eq!(facts.len(), watching.sent.len());
    for (fact, (frame, codec)) in facts.iter().zip(sent) {
        assert_eq!(
            (fact.number, fact.idr, fact.hevc),
            (frame.number, frame.idr, *codec == Codec::Hevc)
        );
    }
}

// On the software encoder for good: a step down has nothing to step to and
// a step up nothing to come back to, and a new frame rate goes no higher
// than 60 fps, each new encoder the software one again.
fn steps_stay_on_software(sharer: &mut Sharer, watching: &mut Watching, size: (u32, u32)) {
    let mut said = Vec::new();
    assert_eq!(sharer.step_down(&mut |line| said.push(line)), Ok(false));
    assert_eq!(sharer.step_up(&mut |line| said.push(line)), Ok(false));
    assert_eq!(sharer.set_fps(30, &mut |line| said.push(line)), Ok(true));
    assert_eq!((sharer.size(), sharer.fps()), (size, 30));
    let (frame, codec) = next_frame(sharer, watching);
    assert!(frame.idr && codec == Codec::H264);
    assert_eq!(sharer.set_fps(120, &mut |line| said.push(line)), Ok(true));
    assert_eq!((sharer.size(), sharer.fps()), (size, 60));
    assert_eq!(sharer.set_fps(144, &mut |line| said.push(line)), Ok(false));
    let (frame, codec) = next_frame(sharer, watching);
    assert!(frame.idr && codec == Codec::H264);
    assert_eq!(sharer.kind(), Some(Kind::MfSoftware));
    assert!(said.is_empty(), "{said:?}");
}

// The GPU encoder gives frame 0 and never asks for another, as Intel's HEVC
// encoder did on the Iris Xe laptop (encode::fault makes NVIDIA's Media
// Foundation encoders do the same), in HEVC and in H.264. The share goes on
// with Windows' software encoder with frame 1, that same frame, as an IDR:
// the GPU encoder shut down and gone before the software one starts, no
// frame number skipped or sent twice, every header saying its codec, the
// way a viewer learns of any codec change, and no GPU encoder again for the
// rest of the share, whatever its viewers take.
#[test]
fn a_stalled_gpu_encoder_falls_back_to_software() {
    let Some((_turn, adapter)) = gpu() else {
        return;
    };
    for codec in [None, Some(Codec::H264)] {
        let before = encode::fault::live();
        let stalls = codec.unwrap_or(Codec::Hevc);
        encode::fault::stall(stalls, 1);
        let packets: Packets = Arc::default();
        let Some(mut sharer) = hardware_share(adapter.clone(), &packets, (1920, 1080), 60, codec)
        else {
            encode::fault::clear();
            continue;
        };
        assert_eq!(encode::fault::live(), before + 1);
        let mut watching = Watching {
            takes_hevc: true,
            ..Watching::default()
        };
        let mut sent = vec![next_frame(&mut sharer, &mut watching)];
        sent.push(next_frame(&mut sharer, &mut watching));
        // Only the software encoder is left.
        assert_eq!(encode::fault::live(), before);
        for _ in 0..4 {
            sent.push(next_frame(&mut sharer, &mut watching));
        }
        let (line, took) = first_software_frame(&watching.lines);
        println!("{stalls}: {line}; said {:#?}", watching.lines);
        let codecs: Vec<Codec> = sent.iter().map(|(_, codec)| *codec).collect();
        let (numbers, idrs) = numbers_and_idrs(&sent);
        assert_eq!(numbers, [0, 1, 2, 3, 4, 5]);
        assert_eq!(codecs[0], stalls);
        assert!(codecs[1..].iter().all(|c| *c == Codec::H264), "{codecs:?}");
        assert_eq!(idrs, [0, 1]);
        assert_eq!(sharer.kind(), Some(Kind::MfSoftware));
        assert_eq!(sharer.software(), Some(Software::GpuFailed));
        assert_eq!((sharer.size(), sharer.fps()), ((1920, 1080), 60));
        assert_eq!(
            sharer.numbers().codec_changes,
            u64::from(stalls == Codec::Hevc)
        );
        let lines = texts(&watching.lines);
        assert!(
            lines.iter().any(|line| line.contains(
                "could not encode frame 1: the hardware encoder has not asked for a frame in 2 s"
            ) && line
                .ends_with("the software encoder takes the share over until it ends")),
            "{lines:#?}"
        );
        assert!(line.contains("ms to encode it again, as an IDR"), "{line}");
        assert!(took < 1000.0, "{line}");
        check_headers(&packets, &watching, &sent);

        // Every viewer takes HEVC, and the share stays on the software
        // encoder past the gap in which it would otherwise go back to it.
        let back = Instant::now();
        while back.elapsed() < SWITCH_GAP + Duration::from_millis(500) {
            let (_, codec) = next_frame(&mut sharer, &mut watching);
            assert_eq!(codec, Codec::H264);
        }
        assert_eq!(sharer.kind(), Some(Kind::MfSoftware));
        steps_stay_on_software(&mut sharer, &mut watching, (1920, 1080));
        assert_eq!(encode::fault::live(), before);
        sharer.finish();
    }
}

// 1440p120 is more than the software encoder takes, so the source opens
// again at its fit, 1080p60, and the next picture, the first from the new
// source, goes out as an IDR under the number of the frame that failed. A
// share stepped down to 1080p60 before its encoder stalls goes on at that
// size, and a step up then leaves it there.
#[test]
fn a_stall_at_1440p120_falls_back_at_1080p60() {
    let Some((_turn, adapter)) = gpu() else {
        return;
    };
    let before = encode::fault::live();
    encode::fault::stall(Codec::H264, 2);
    let packets: Packets = Arc::default();
    let Some(mut sharer) = hardware_share(
        adapter.clone(),
        &packets,
        (2560, 1440),
        120,
        Some(Codec::H264),
    ) else {
        encode::fault::clear();
        return;
    };
    assert_eq!((sharer.size(), sharer.fps()), ((2560, 1440), 120));
    let mut watching = Watching::default();
    let mut sent = Vec::new();
    for _ in 0..5 {
        sent.push(next_frame(&mut sharer, &mut watching));
    }
    assert_eq!(encode::fault::live(), before);
    let (line, took) = first_software_frame(&watching.lines);
    println!("{line}; said {:#?}", watching.lines);
    let (numbers, idrs) = numbers_and_idrs(&sent);
    assert_eq!(numbers, [0, 1, 2, 3, 4]);
    assert_eq!(idrs, [0, 2]);
    assert_eq!((sharer.size(), sharer.fps()), ((1920, 1080), 60));
    assert_eq!(sharer.software(), Some(Software::GpuFailed));
    let lines = texts(&watching.lines);
    assert!(lines.contains(
        &"the software encoder takes up to 1080p at 60 fps, so this share runs at 1920x1080 and 60 fps instead of 2560x1440 and 120"
    ));
    assert!(
        line.contains("open the source again at 1920x1080 and 60 fps with the software encoder")
            && line.contains("the new source's first picture, as an IDR"),
        "{line}"
    );
    assert!(took < 1000.0, "{line}");
    check_headers(&packets, &watching, &sent);
    steps_stay_on_software(&mut sharer, &mut watching, (1920, 1080));
    sharer.finish();

    // Stepped down first, on the GPU encoder.
    let packets: Packets = Arc::default();
    let Some(mut sharer) = hardware_share(adapter, &packets, (2560, 1440), 120, Some(Codec::H264))
    else {
        return;
    };
    let mut watching = Watching::default();
    next_frame(&mut sharer, &mut watching);
    encode::fault::stall(Codec::H264, 1);
    assert_eq!(sharer.step_down(&mut |_| {}), Ok(true));
    assert_eq!((sharer.size(), sharer.fps()), ((1920, 1080), 60));
    assert_eq!(sharer.kind(), Some(Kind::MfHardware));
    next_frame(&mut sharer, &mut watching);
    let (frame, codec) = next_frame(&mut sharer, &mut watching);
    assert_eq!((frame.number, frame.idr, codec), (2, true, Codec::H264));
    assert_eq!(sharer.kind(), Some(Kind::MfSoftware));
    let (line, _) = first_software_frame(&watching.lines);
    assert!(line.contains("encode it again, as an IDR"), "{line}");
    assert!(sharer.stepped_down());
    assert_eq!(sharer.step_up(&mut |_| {}), Ok(false));
    assert!(!sharer.stepped_down());
    assert_eq!((sharer.size(), sharer.fps()), ((1920, 1080), 60));
    steps_stay_on_software(&mut sharer, &mut watching, (1920, 1080));
    assert_eq!(encode::fault::live(), before);
    sharer.finish();
}

// NVENC reports an error for a frame (encode::fault makes the driver's
// release of the output fail after the frame was encoded): the software
// encoder makes that same frame again as an IDR. In HEVC at 1440p120 the
// share also changes codec and size, and its next picture is the IDR.
#[test]
fn a_failed_nvenc_falls_back_to_software() {
    let Some((_turn, adapter)) = gpu() else {
        return;
    };
    if adapter.vendor_id != NVIDIA {
        println!("skipped: no NVIDIA GPU here, so no NVENC to fail");
        return;
    }
    for (size, fps, codec, fails_at) in [
        ((1920, 1080), 60, Some(Codec::H264), 3),
        ((2560, 1440), 120, None, 1),
    ] {
        encode::fault::fail_nvenc(fails_at);
        let packets: Packets = Arc::default();
        let (mut sharer, _) = share_on(adapter.clone(), &packets, size, fps, None, codec)
            .unwrap_or_else(|err| panic!("{err}"));
        if !sharer.encoder_name().starts_with("NVENC") {
            encode::fault::clear();
            println!("skipped: NVENC did not open at {size:?}");
            continue;
        }
        let started = sharer.codec().expect("an encoder");
        let mut watching = Watching {
            takes_hevc: true,
            ..Watching::default()
        };
        let mut sent = Vec::new();
        for _ in 0..fails_at + 3 {
            sent.push(next_frame(&mut sharer, &mut watching));
        }
        let (line, took) = first_software_frame(&watching.lines);
        println!("{started} at {size:?}: {line}; said {:#?}", watching.lines);
        let (numbers, idrs) = numbers_and_idrs(&sent);
        assert_eq!(numbers, (0..fails_at as u32 + 3).collect::<Vec<_>>());
        assert_eq!(idrs, [0, fails_at as u32]);
        assert!(
            sent[fails_at as usize..]
                .iter()
                .all(|(_, codec)| *codec == Codec::H264)
        );
        assert_eq!(sharer.kind(), Some(Kind::MfSoftware));
        assert_eq!(sharer.software(), Some(Software::GpuFailed));
        assert!(
            texts(&watching.lines).iter().any(|line| line.starts_with(&format!(
                "NVENC {started} P1 could not encode frame {fails_at}: NVENC could not release its output buffer"
            ))),
            "{:#?}",
            watching.lines
        );
        assert!(took < 1000.0, "{line}");
        // NVENC's frames survive a loss by invalidation; the software
        // encoder's do not, and the header says so from its first.
        let count = watching.sent.iter().map(|frame| frame.packets).sum();
        let facts = frames(&wait_for(&packets, count));
        let survives: Vec<bool> = facts.iter().map(|fact| fact.survives_loss).collect();
        assert!(
            survives[..fails_at as usize].iter().all(|s| *s),
            "{survives:?}"
        );
        assert!(
            survives[fails_at as usize..].iter().all(|s| !s),
            "{survives:?}"
        );
        if size == (2560, 1440) {
            assert_eq!((sharer.size(), sharer.fps()), ((1920, 1080), 60));
            assert!(line.contains("the new source's first picture"), "{line}");
        } else {
            assert!(line.contains("encode it again"), "{line}");
        }
        sharer.finish();
    }
}

// The software encoder that takes over does not open, or fails on its first
// frame or a later one: the share ends, saying what happened and what to
// do, as the panel shows it after "Sharing stopped: ". The log has the
// encoder's own error, and after a first frame how long the change took,
// which shows whether the GPU encoder really shut down.
#[test]
fn software_failing_too_ends_the_share() {
    let Some((_turn, adapter)) = gpu() else {
        return;
    };
    for software_frames in [None, Some(0), Some(2)] {
        let before = encode::fault::live();
        encode::fault::stall(Codec::H264, 1);
        match software_frames {
            None => encode::fault::refuse_software(),
            Some(frames) => encode::fault::fail_software(frames),
        }
        let packets: Packets = Arc::default();
        let Some(mut sharer) = hardware_share(
            adapter.clone(),
            &packets,
            (1280, 720),
            60,
            Some(Codec::H264),
        ) else {
            encode::fault::clear();
            return;
        };
        let mut watching = Watching::default();
        let ended = loop {
            if let Err(why) = sharer.next(&mut watching) {
                break why;
            }
        };
        println!("{ended}; said {:#?}", watching.lines);
        let numbers: Vec<u32> = watching.sent.iter().map(|frame| frame.number).collect();
        let lines = texts(&watching.lines);
        let logged = |text: &str| lines.iter().any(|line| line.contains(text));
        match software_frames {
            None => {
                assert_eq!(ended, SOFTWARE_DID_NOT_START);
                assert_eq!(numbers, [0]);
                assert!(
                    logged(
                        "the software encoder did not open after Media Foundation hardware H.264"
                    ) && logged("Media Foundation could not start Windows' software H.264 encoder"),
                    "{lines:#?}"
                );
            }
            Some(frames) => {
                assert_eq!(ended, SOFTWARE_FAILED_TOO);
                let failed = 1 + frames as u32;
                assert_eq!(numbers, (0..failed).collect::<Vec<_>>());
                let then = match frames {
                    0 => ", its first (",
                    _ => ": ",
                };
                assert!(
                    lines.iter().any(|line| line.starts_with(
                        "Media Foundation H.264, software, 1080p60 could not encode frame"
                    ) && line.contains(&format!("frame {failed}{then}"))
                        && line.contains(
                            "Media Foundation could not feed a frame to the software encoder"
                        )),
                    "{lines:#?}"
                );
            }
        }
        drop(sharer);
        assert_eq!(encode::fault::live(), before);
        encode::fault::clear();
    }
}

// The pattern's adapter said to be Intel's, on this PC's GPU: the choice
// reads the vendor, the encoders the device. A room's share starts on the
// software encoder at its fit, from frame 0, and stays there; the loopback's
// --encoder and --codec still open what they ask for.
#[test]
fn a_share_on_intel_graphics_starts_on_the_software_encoder() {
    let Some((_turn, adapter)) = gpu() else {
        return;
    };
    let intel = capture::Adapter {
        vendor_id: INTEL,
        ..adapter
    };
    let packets: Packets = Arc::default();
    let (mut sharer, said) = share_on(intel.clone(), &packets, (2560, 1440), 120, None, None)
        .unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(sharer.kind(), Some(Kind::MfSoftware));
    assert_eq!(sharer.software(), Some(Software::Intel));
    assert_eq!((sharer.size(), sharer.fps()), ((1920, 1080), 60));
    // Then the software encoder's notes, as any encoder's at open.
    assert_eq!(
        texts(&said)[..2],
        [
            format!(
                "the GPU is Intel's ({}), whose hardware encoders Booth does not use yet, so this share uses the software encoder",
                intel.description
            )
            .as_str(),
            "the software encoder takes up to 1080p at 60 fps, so this share runs at 1920x1080 and 60 fps instead of 2560x1440 and 120",
        ],
        "{said:?}"
    );
    let mut watching = Watching {
        takes_hevc: true,
        ..Watching::default()
    };
    let mut sent = Vec::new();
    for _ in 0..5 {
        sent.push(next_frame(&mut sharer, &mut watching));
    }
    let (numbers, idrs) = numbers_and_idrs(&sent);
    assert_eq!(numbers, [0, 1, 2, 3, 4]);
    assert_eq!(idrs, [0]);
    assert!(sent.iter().all(|(_, codec)| *codec == Codec::H264));
    steps_stay_on_software(&mut sharer, &mut watching, (1920, 1080));
    assert_eq!(sharer.software(), Some(Software::Intel));
    sharer.finish();

    for (encoder, codec) in [
        (Some(Kind::MfHardware), None),
        (None, Some(Codec::H264)),
        (None, Some(Codec::Hevc)),
    ] {
        let (sharer, _) = match share_on(intel.clone(), &packets, (1920, 1080), 60, encoder, codec)
        {
            Ok(opened) => opened,
            Err(err) => {
                println!("skipped {encoder:?} {codec:?}: {err}");
                continue;
            }
        };
        assert_ne!(
            sharer.kind(),
            Some(Kind::MfSoftware),
            "{encoder:?} {codec:?}"
        );
        assert_eq!(sharer.software(), None);
        sharer.finish();
    }
}
