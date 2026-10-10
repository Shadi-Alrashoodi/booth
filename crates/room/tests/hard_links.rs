// A host who talks and shares the test pattern, and a friend who talks back
// and watches it, joined through a link as bad as a home connection gets:
// loss at random and in bursts, jitter, a narrow pipe, a long outage. Each
// run prints, phase by phase, what the friend saw and what each side heard,
// and whether the room came back by itself once the link did.
//
// Loopback is a LAN path to the room, so the share sends LAN-sized packets
// and never steps down for a low rate, as it would over the internet. The
// timers are the real ones: reconnecting after 3 s, lost after 15.
//
// Not run with the rest: each takes minutes and the GPU, and most of what
// they measure is read, not asserted. The two outage runs assert that voice
// and the picture come back once the link does.
//
//   cargo test -p room --test hard_links -- --ignored --nocapture --test-threads 1

mod common;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use common::voiced::{RATE, Voiced, settled, tone_440, tone_660, voiced};
use common::{invite_to, loopback};
use net::Socket;
use room::view::{LinkState, OwnShare, View};
use room::{Show, TalkMode, Timers, VideoConfig, VideoSource};
use voice::audio::fake::Fake;

const NVIDIA: u32 = 0x10de;
const FFMPEG: [&str; 2] = ["avcodec-62.dll", "avutil-60.dll"];
const WAIT: Duration = Duration::from_secs(10);

// How much a home router queues before it drops.
const QUEUE: Duration = Duration::from_millis(200);
// Wi-Fi's delay comes in stretches, so one draw of the jitter holds this
// long for every packet in it.
const JITTER_HOLDS: Duration = Duration::from_millis(50);
// Shorter silences are a frame or two of loss the decoder covered, or the
// tone crossing zero; this long is a gap anyone hears.
const GAP: Duration = Duration::from_millis(20);
const QUIET: f32 = 1e-4;

// False, with the reason printed, when this PC cannot run the video path.
fn ready() -> bool {
    let adapters = share::adapters().unwrap_or_else(|err| panic!("{err}"));
    if !adapters.iter().any(|adapter| adapter.vendor_id == NVIDIA) {
        println!("skipped: no NVIDIA GPU on this PC, so no NVENC to share the pattern with");
        return false;
    }
    let exe = std::env::current_exe().expect("the test's own path");
    let folder = exe.parent().expect("the test's folder");
    if let Some(missing) = FFMPEG.iter().find(|dll| !folder.join(dll).is_file()) {
        println!(
            "skipped: {missing} is not in {}; build FFmpeg with powershell -ExecutionPolicy Bypass -File tools\\build-ffmpeg.ps1 and build again",
            folder.display()
        );
        return false;
    }
    share::make_process_dpi_aware().unwrap_or_else(|err| panic!("{err}"));
    true
}

fn pattern() -> VideoConfig {
    VideoConfig {
        source: VideoSource::Pattern {
            width: 2560,
            height: 1440,
            busy: false,
        },
        show: Show::NoActivate,
        vsync: false,
        loss: None,
        hevc: true,
        injector: None,
        capture: None,
    }
}

// What one direction of the link does to each packet.
#[derive(Clone, Copy, Debug, Default)]
struct Shape {
    // Each packet dropped on its own at random.
    loss_pct: u32,
    // Every 2 to 6 s everything is dropped for 100 to 500 ms.
    bursts: bool,
    // Extra one-way delay, drawn evenly from 0 to this for each stretch of
    // JITTER_HOLDS. Nothing overtakes.
    jitter_ms: u64,
    // A link of this rate with a QUEUE of buffer, which drops what does not
    // fit.
    capacity_kbps: Option<u64>,
    blocked: bool,
}

impl Shape {
    fn loss(loss_pct: u32) -> Shape {
        Shape {
            loss_pct,
            ..Shape::default()
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Tally {
    passed: u64,
    random: u64,
    burst: u64,
    queue: u64,
    blocked: u64,
}

impl Tally {
    fn since(&self, before: &Tally) -> Tally {
        Tally {
            passed: self.passed - before.passed,
            random: self.random - before.random,
            burst: self.burst - before.burst,
            queue: self.queue - before.queue,
            blocked: self.blocked - before.blocked,
        }
    }

    fn lost_pct(&self) -> f64 {
        let lost = self.random + self.burst + self.queue + self.blocked;
        let all = lost + self.passed;
        if all == 0 {
            0.0
        } else {
            lost as f64 * 100.0 / all as f64
        }
    }
}

// xorshift64*, so a run draws the same whatever the timing.
fn roll(dice: &mut u64) -> u64 {
    *dice ^= *dice >> 12;
    *dice ^= *dice << 25;
    *dice ^= *dice >> 27;
    dice.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

fn between(dice: &mut u64, least: Duration, most: Duration) -> Duration {
    let spread = (most - least).as_micros() as u64;
    least + Duration::from_micros(roll(dice) % (spread + 1))
}

struct Way {
    shape: Shape,
    dice: u64,
    // When the link of capacity_kbps is done with what it holds.
    busy_until: Instant,
    // When the last packet is due, which the next one never beats.
    last_due: Instant,
    // The jitter drawn last, and until when it holds.
    held: (Instant, Duration),
    burst: Option<(Instant, Instant)>,
    tally: Tally,
}

impl Way {
    fn new(seed: u64) -> Way {
        let now = Instant::now();
        Way {
            shape: Shape::default(),
            dice: seed,
            busy_until: now,
            last_due: now,
            held: (now, Duration::ZERO),
            burst: None,
            tally: Tally::default(),
        }
    }

    fn next_burst(&mut self, now: Instant) -> (Instant, Instant) {
        let start = now
            + between(
                &mut self.dice,
                Duration::from_secs(2),
                Duration::from_secs(6),
            );
        let end = start
            + between(
                &mut self.dice,
                Duration::from_millis(100),
                Duration::from_millis(500),
            );
        (start, end)
    }

    // When the packet arrives, or None when the link drops it.
    fn due(&mut self, now: Instant, len: usize) -> Option<Instant> {
        let shape = self.shape;
        if shape.blocked {
            self.tally.blocked += 1;
            return None;
        }
        if shape.bursts {
            let (start, end) = match self.burst {
                Some((_, end)) if now >= end => self.next_burst(now),
                Some(burst) => burst,
                None => self.next_burst(now),
            };
            self.burst = Some((start, end));
            if now >= start {
                self.tally.burst += 1;
                return None;
            }
        } else {
            self.burst = None;
        }
        if shape.loss_pct > 0 && roll(&mut self.dice) % 100 < u64::from(shape.loss_pct) {
            self.tally.random += 1;
            return None;
        }
        let mut arrives = now;
        if let Some(kbps) = shape.capacity_kbps {
            let start = self.busy_until.max(now);
            if start.saturating_duration_since(now) > QUEUE {
                self.tally.queue += 1;
                return None;
            }
            // The IPv4 and UDP headers go through the link too.
            let bits = (len as u64 + 28) * 8;
            self.busy_until = start + Duration::from_micros(bits * 1000 / kbps);
            arrives = self.busy_until;
        }
        if shape.jitter_ms > 0 {
            if now >= self.held.0 {
                let extra = Duration::from_millis(roll(&mut self.dice) % (shape.jitter_ms + 1));
                self.held = (now + JITTER_HOLDS, extra);
            }
            arrives += self.held.1;
        }
        self.last_due = self.last_due.max(arrives);
        self.tally.passed += 1;
        Some(self.last_due)
    }
}

struct Ways {
    stop: AtomicBool,
    to_host: Mutex<Way>,
    to_friend: Mutex<Way>,
    friend: Mutex<Option<SocketAddr>>,
    friend_side: Socket,
    host_side: Socket,
    host: SocketAddr,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// Between the friend and the host: the friend sends to `addr`, and each way
// has a Shape of its own.
struct Link {
    addr: SocketAddr,
    ways: Arc<Ways>,
    threads: Vec<JoinHandle<()>>,
}

impl Link {
    fn new(host: SocketAddr) -> Link {
        let friend_side = Socket::bind(0).expect("bind the link's friend side");
        let addr = loopback(friend_side.local_port());
        let ways = Arc::new(Ways {
            stop: AtomicBool::new(false),
            to_host: Mutex::new(Way::new(0x9E37_79B9_7F4A_7C15)),
            to_friend: Mutex::new(Way::new(0xD1B5_4A32_D192_ED03)),
            friend: Mutex::new(None),
            friend_side,
            host_side: Socket::bind(0).expect("bind the link's host side"),
            host,
        });
        let mut threads = Vec::new();
        for toward_host in [true, false] {
            let (held, due) = mpsc::channel::<(Instant, Vec<u8>)>();
            let reader = Arc::clone(&ways);
            threads.push(thread::spawn(move || {
                let mut buf = [0u8; 2048];
                let (from, way) = if toward_host {
                    (&reader.friend_side, &reader.to_host)
                } else {
                    (&reader.host_side, &reader.to_friend)
                };
                while !reader.stop.load(Ordering::Acquire) {
                    let Ok((len @ 1.., sender)) = from.recv_from(&mut buf) else {
                        continue;
                    };
                    if toward_host {
                        *lock(&reader.friend) = Some(sender);
                    }
                    let Some(at) = lock(way).due(Instant::now(), len) else {
                        continue;
                    };
                    if held.send((at, buf[..len].to_vec())).is_err() {
                        break;
                    }
                }
            }));
            let sender = Arc::clone(&ways);
            threads.push(thread::spawn(move || {
                for (at, packet) in due {
                    thread::sleep(at.saturating_duration_since(Instant::now()));
                    if toward_host {
                        let _ = sender.host_side.send_to(&packet, sender.host);
                    } else if let Some(friend) = *lock(&sender.friend) {
                        let _ = sender.friend_side.send_to(&packet, friend);
                    }
                }
            }));
        }
        Link {
            addr,
            ways,
            threads,
        }
    }

    fn shape(&self, to_friend: Shape, to_host: Shape) {
        lock(&self.ways.to_friend).shape = to_friend;
        lock(&self.ways.to_host).shape = to_host;
    }

    fn tallies(&self) -> (Tally, Tally) {
        (
            lock(&self.ways.to_friend).tally,
            lock(&self.ways.to_host).tally,
        )
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.ways.stop.store(true, Ordering::Release);
        let _ = self.ways.friend_side.wake();
        let _ = self.ways.host_side.wake();
        // The readers end, which ends the senders' channels.
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

// One stretch of a run: how long, and what the link does each way.
struct Phase {
    name: &'static str,
    secs: u64,
    to_friend: Shape,
    to_host: Shape,
}

fn both(name: &'static str, secs: u64, shape: Shape) -> Phase {
    Phase {
        name,
        secs,
        to_friend: shape,
        to_host: shape,
    }
}

// One second of the friend's view and the host's.
#[derive(Clone, Copy, Debug)]
struct Second {
    fps: u32,
    watching: bool,
    live: bool,
    host_sees_her: bool,
    rate_kbps: Option<u32>,
    encoded_fps: Option<u32>,
    decode_ms: Option<f32>,
    encode_ms: Option<f32>,
}

fn own_share(view: &View) -> Option<u32> {
    match view.share.own {
        OwnShare::Sharing { number, .. } => Some(number),
        _ => None,
    }
}

fn second(friend: &View, host: &View) -> Second {
    Second {
        fps: friend
            .numbers
            .watching
            .as_ref()
            .filter(|_| friend.share.watching)
            .map_or(0, |w| w.fps),
        watching: friend.share.watching && friend.share.viewer_open,
        live: friend.strip.state == LinkState::Live,
        host_sees_her: host
            .people
            .iter()
            .any(|p| !p.is_you && p.name == "Ana" && p.rtt_ms.is_some()),
        rate_kbps: host.numbers.sharing.as_ref().map(|s| s.rate_kbps),
        encoded_fps: host.numbers.sharing.as_ref().map(|s| s.encoded_fps),
        decode_ms: friend.numbers.watching.as_ref().and_then(|w| w.decode_ms),
        encode_ms: host
            .numbers
            .sharing
            .as_ref()
            .and_then(|s| s.encode_ms)
            .map(|l| l.median_ms),
    }
}

// Silences of GAP or longer in mono samples: their total and the longest,
// in milliseconds, and how many.
#[derive(Debug, Default)]
struct Gaps {
    total_ms: f64,
    longest_ms: f64,
    count: usize,
}

fn gaps(samples: &[f32]) -> Gaps {
    let least = (GAP.as_secs_f64() * RATE as f64) as usize;
    let ms = |n: usize| n as f64 * 1000.0 / RATE as f64;
    let mut out = Gaps::default();
    let mut run = 0usize;
    let close = |run: usize, out: &mut Gaps| {
        if run >= least {
            out.total_ms += ms(run);
            out.longest_ms = out.longest_ms.max(ms(run));
            out.count += 1;
        }
    };
    for sample in samples {
        if sample.abs() < QUIET {
            run += 1;
        } else {
            close(run, &mut out);
            run = 0;
        }
    }
    close(run, &mut out);
    out
}

fn mono_len(speakers: &Fake) -> usize {
    speakers.record().written.len() / 2
}

fn mono(speakers: &Fake) -> Vec<f32> {
    speakers
        .record()
        .written
        .iter()
        .step_by(2)
        .copied()
        .collect()
}

struct Report {
    name: &'static str,
    seconds: Vec<Second>,
    links: (Tally, Tally),
    // Each side's speakers, in mono samples since they opened: where the
    // phase starts and ends.
    friend_heard: (usize, usize),
    host_heard: (usize, usize),
    // Seconds into the phase at which Watch was pressed again.
    watched_again: Option<usize>,
}

fn print_report(report: &Report, friend_audio: &[f32], host_audio: &[f32]) {
    let seconds = &report.seconds;
    let dark = seconds.iter().filter(|s| s.fps == 0).count();
    let slow = seconds.iter().filter(|s| s.fps > 0 && s.fps < 10).count();
    let shown: u64 = seconds.iter().map(|s| u64::from(s.fps)).sum();
    let not_live = seconds.iter().filter(|s| !s.live).count();
    let not_watching = seconds.iter().filter(|s| !s.watching).count();
    let host_lost_her = seconds.iter().filter(|s| !s.host_sees_her).count();
    let rates: Vec<u32> = seconds.iter().filter_map(|s| s.rate_kbps).collect();
    let rate = if rates.is_empty() {
        String::from("no share running")
    } else {
        format!(
            "share rate {} to {} kbit/s",
            rates.iter().min().unwrap(),
            rates.iter().max().unwrap()
        )
    };
    let slice = |audio: &[f32], (from, to): (usize, usize)| -> Gaps {
        gaps(&audio[from.min(audio.len())..to.min(audio.len())])
    };
    let to_friend = slice(friend_audio, report.friend_heard);
    let to_host = slice(host_audio, report.host_heard);
    let (down, up) = report.links;
    println!(
        "  {}: {} s, link toward the friend lost {:.1}% (random {}, bursts {}, queue {}, outage {}), toward the host {:.1}%",
        report.name,
        seconds.len(),
        down.lost_pct(),
        down.random,
        down.burst,
        down.queue,
        down.blocked,
        up.lost_pct()
    );
    println!(
        "    picture: {dark} s with no frame, {slow} s under 10 fps, {shown} frames in all; friend not live {not_live} s, not watching {not_watching} s, host without her {host_lost_her} s; {rate}{}",
        report
            .watched_again
            .map_or_else(String::new, |at| format!("; Watch pressed again at {at} s"))
    );
    println!(
        "    voice to the friend: {:.0} ms of gaps in all, longest {:.0} ms, {} gaps; to the host: {:.0} ms, longest {:.0} ms, {} gaps",
        to_friend.total_ms,
        to_friend.longest_ms,
        to_friend.count,
        to_host.total_ms,
        to_host.longest_ms,
        to_host.count
    );
    let fps: Vec<String> = seconds.iter().map(|s| s.fps.to_string()).collect();
    println!("    fps by second: {}", fps.join(" "));
    // Whether a falling frame rate is the link or the sharer: a light share
    // can let the GPU clock down until encoding slows.
    let sharer: Vec<String> = seconds
        .iter()
        .map(|s| {
            format!(
                "{}/{:.1}/{:.1}",
                s.encoded_fps.unwrap_or(0),
                s.encode_ms.unwrap_or(0.0),
                s.decode_ms.unwrap_or(0.0)
            )
        })
        .collect();
    println!(
        "    encoded fps / encode ms / decode ms by second: {}",
        sharer.join(" ")
    );
}

// When, after `from`, sound first comes back in `audio` and stays for a
// second, in ms after `from`. Silences shorter than GAP do not break the
// second: the tone crosses zero, and the decoder covers a lost frame.
fn sound_back(audio: &[f32], from: usize) -> Option<f64> {
    let hold = RATE as usize;
    let gap = (GAP.as_secs_f64() * RATE as f64) as usize;
    let mut start = None;
    let mut quiet = 0usize;
    for (at, sample) in audio.iter().enumerate().skip(from) {
        if sample.abs() < QUIET {
            quiet += 1;
            if quiet >= gap {
                start = None;
            }
            continue;
        }
        quiet = 0;
        let began = *start.get_or_insert(at);
        if at - began >= hold {
            return Some((began - from) as f64 * 1000.0 / RATE as f64);
        }
    }
    None
}

struct Run {
    reports: Vec<Report>,
    friend_audio: Vec<f32>,
    host_audio: Vec<f32>,
}

fn run(title: &str, phases: &[Phase]) -> Run {
    println!("{title}");
    let timers = Timers::default();
    let (mut host_config, host_mic, host_speakers) =
        voiced("Mara", timers, tone_440, TalkMode::PushToTalk, true);
    host_config.video = pattern();
    let host = Voiced::host_with((host_config, host_mic, host_speakers));
    let link = Link::new(loopback(host.member.port()));
    let (mut ana_config, ana_mic, ana_speakers) =
        voiced("Ana", timers, tone_660, TalkMode::PushToTalk, true);
    ana_config.video = pattern();
    let ana = Voiced::join(
        (ana_config, ana_mic, ana_speakers),
        invite_to(&host.member, link.addr),
    );
    settled(&host, &[&ana]);
    host.room().talk(true);
    ana.room().talk(true);
    host.room().share(120, None);
    let share = own_share(&host.member.wait_for(WAIT, "the share running", |v| {
        own_share(v).is_some() && v.share.running.is_some()
    }))
    .expect("the share");
    ana.member.wait_for(WAIT, "the share in the roster", |v| {
        v.share.current.as_ref().is_some_and(|c| c.number == share)
    });
    ana.room().watch(share, true);
    ana.member.wait_for(WAIT, "the viewer open", |v| {
        v.share.watching && v.share.viewer_open
    });
    // A few seconds clean first, so every phase starts from a settled share.
    thread::sleep(Duration::from_secs(3));

    let mut reports = Vec::new();
    for phase in phases {
        link.shape(phase.to_friend, phase.to_host);
        let tallies = link.tallies();
        let friend_from = mono_len(&ana.speakers);
        let host_from = mono_len(&host.speakers);
        let start = Instant::now();
        let mut seconds = Vec::new();
        let mut watched_again = None;
        for at in 1..=phase.secs {
            thread::sleep(
                (start + Duration::from_secs(at)).saturating_duration_since(Instant::now()),
            );
            let friend = ana.view();
            let seen = second(&friend, &host.view());
            // A friend who was lost has no share to watch any more; once she
            // is back and the share is in her roster, she presses Watch, as a
            // person would.
            if !friend.share.watching
                && friend.strip.state == LinkState::Live
                && friend
                    .share
                    .current
                    .as_ref()
                    .is_some_and(|c| c.number == share)
                && watched_again.is_none()
            {
                ana.room().watch(share, true);
                watched_again = Some(at as usize);
            }
            seconds.push(seen);
        }
        let now = link.tallies();
        reports.push(Report {
            name: phase.name,
            seconds,
            links: (now.0.since(&tallies.0), now.1.since(&tallies.1)),
            friend_heard: (friend_from, mono_len(&ana.speakers)),
            host_heard: (host_from, mono_len(&host.speakers)),
            watched_again,
        });
    }
    host.room().talk(false);
    ana.room().talk(false);
    let friend_audio = mono(&ana.speakers);
    let host_audio = mono(&host.speakers);
    for report in &reports {
        print_report(report, &friend_audio, &host_audio);
    }
    Run {
        reports,
        friend_audio,
        host_audio,
    }
}

#[test]
#[ignore = "minutes on the GPU; run on its own with --nocapture"]
fn loss_at_20_and_30_percent() {
    if !ready() {
        return;
    }
    run(
        "random loss both ways",
        &[
            both("clean", 10, Shape::default()),
            both("20% loss", 60, Shape::loss(20)),
            both("clean again", 10, Shape::default()),
            both("30% loss", 60, Shape::loss(30)),
            both("clean again", 15, Shape::default()),
        ],
    );
}

#[test]
#[ignore = "minutes on the GPU; run on its own with --nocapture"]
fn jitter_of_200_ms() {
    if !ready() {
        return;
    }
    let jitter = Shape {
        jitter_ms: 200,
        ..Shape::default()
    };
    run(
        "0 to 200 ms of extra delay both ways, a new draw every 50 ms",
        &[
            both("clean", 10, Shape::default()),
            both("jitter 200 ms", 60, jitter),
            both("clean again", 20, Shape::default()),
        ],
    );
}

#[test]
#[ignore = "minutes on the GPU; run on its own with --nocapture"]
fn loss_in_bursts() {
    if !ready() {
        return;
    }
    let bursts = Shape {
        bursts: true,
        loss_pct: 2,
        ..Shape::default()
    };
    run(
        "bursts of 100 to 500 ms every 2 to 6 s, and 2% at random, both ways",
        &[
            both("clean", 10, Shape::default()),
            both("bursts", 90, bursts),
            both("clean again", 20, Shape::default()),
        ],
    );
}

#[test]
#[ignore = "minutes on the GPU; run on its own with --nocapture"]
fn bandwidth_down_to_1_mbit_and_back() {
    if !ready() {
        return;
    }
    let narrow = Shape {
        capacity_kbps: Some(1000),
        ..Shape::default()
    };
    run(
        "both ways through 20 Mbit/s, then 1 Mbit/s, then 20 again",
        &[
            both(
                "20 Mbit/s",
                20,
                Shape {
                    capacity_kbps: Some(20_000),
                    ..Shape::default()
                },
            ),
            both("1 Mbit/s", 60, narrow),
            both(
                "20 Mbit/s again",
                60,
                Shape {
                    capacity_kbps: Some(20_000),
                    ..Shape::default()
                },
            ),
        ],
    );
}

#[test]
#[ignore = "minutes on the GPU; run on its own with --nocapture"]
fn outage_of_two_minutes() {
    if !ready() {
        return;
    }
    let dark = Shape {
        blocked: true,
        ..Shape::default()
    };
    let got = run(
        "nothing either way for two minutes",
        &[
            both("clean", 20, Shape::default()),
            both("outage", 120, dark),
            both("after", 60, Shape::default()),
        ],
    );
    let after = &got.reports[2];
    let picture = after.seconds.iter().position(|s| s.fps > 0);
    let live = after.seconds.iter().position(|s| s.live);
    let host_sees = after.seconds.iter().position(|s| s.host_sees_her);
    let voice_to_friend = sound_back(&got.friend_audio, after.friend_heard.0);
    let voice_to_host = sound_back(&got.host_audio, after.host_heard.0);
    println!(
        "  after the outage: friend live at {live:?} s, host sees her at {host_sees:?} s, picture at {picture:?} s, voice to the friend at {voice_to_friend:?} ms, to the host at {voice_to_host:?} ms"
    );
    assert!(live.is_some(), "the friend never came back");
    assert!(picture.is_some(), "the picture never came back");
    assert!(
        voice_to_friend.is_some() && voice_to_host.is_some(),
        "voice did not come back both ways"
    );
}

#[test]
#[ignore = "minutes on the GPU; run on its own with --nocapture"]
fn viewer_offline_for_30_s() {
    if !ready() {
        return;
    }
    let dark = Shape {
        blocked: true,
        ..Shape::default()
    };
    let got = run(
        "the friend's network gone for 30 s",
        &[
            both("clean", 15, Shape::default()),
            both("offline", 30, dark),
            both("after", 30, Shape::default()),
        ],
    );
    let after = &got.reports[2];
    let picture = after.seconds.iter().position(|s| s.fps > 0);
    let live = after.seconds.iter().position(|s| s.live);
    let voice_to_friend = sound_back(&got.friend_audio, after.friend_heard.0);
    let voice_to_host = sound_back(&got.host_audio, after.host_heard.0);
    println!(
        "  after 30 s offline: friend live at {live:?} s, picture at {picture:?} s, voice to the friend at {voice_to_friend:?} ms, to the host at {voice_to_host:?} ms"
    );
    assert!(live.is_some() && picture.is_some());
    assert!(voice_to_friend.is_some() && voice_to_host.is_some());
}
