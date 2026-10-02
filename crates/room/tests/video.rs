// The video path through the room: the room's own share thread captures,
// encodes and sends, and the room's own viewer thread decodes and shows, on
// this PC over loopback with fake voice devices. Nothing of the screen is
// captured: every share here is capture's test pattern.
// Every viewer window opens without taking the focus and closes within
// seconds, and the tests take turns on the GPU.
//
// Skipped, with the reason printed, on a PC without an NVIDIA GPU or without
// the FFmpeg DLLs next to the test.

mod common;

use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use common::voiced::heard_cues;
use common::{Member, code_to_invite, config, loopback, poll, timers};
use room::view::{Codec, LineKind, LinkState, OwnShare, SharingNumbers, View, WatchingNumbers};
use room::{Config, Devices, LossKnob, Show, VideoConfig, VideoSource};
use voice::audio::fake::Fake;

const NVIDIA: u32 = 0x10de;
const FFMPEG: [&str; 2] = ["avcodec-62.dll", "avutil-60.dll"];
const WAIT: Duration = Duration::from_secs(5);

// One test at a time on the GPU, and one at a time raising the timer
// resolution, which fine_timers_held counts for the whole process.
static TURN: Mutex<()> = Mutex::new(());

fn turn() -> MutexGuard<'static, ()> {
    TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

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
    // What winit does for booth.exe; the viewer's window wants it.
    share::make_process_dpi_aware().unwrap_or_else(|err| panic!("{err}"));
    true
}

fn pattern(loss: Option<LossKnob>) -> VideoConfig {
    VideoConfig {
        source: VideoSource::Pattern {
            width: 2560,
            height: 1440,
            busy: false,
        },
        show: Show::NoActivate,
        vsync: false,
        loss,
        hevc: true,
        injector: None,
        capture: None,
    }
}

// The pattern as noise, which sends as much as its rate lets it, small
// enough to draw on the CPU at 120 fps in a debug build.
fn busy() -> VideoConfig {
    VideoConfig {
        source: VideoSource::Pattern {
            width: 640,
            height: 360,
            busy: true,
        },
        ..pattern(None)
    }
}

fn config_with(name: &str, video: VideoConfig, upload_kbps: u32) -> Config {
    let mut config = config(name, timers());
    config.video = video;
    config.video_upload_kbps = upload_kbps;
    config
}

// A watcher that says it takes no HEVC, standing in for a GPU without it.
fn without_hevc(mut config: Config) -> Config {
    config.video.hevc = false;
    config
}

// Whether this PC's viewer decodes HEVC, as a room asks when it opens. A
// test that needs it says why it is skipped when it does not.
fn takes_hevc() -> bool {
    match share::primary_takes_hevc() {
        Ok(()) => true,
        Err(why) => {
            println!("skipped: this PC's viewer does not take HEVC: {why}");
            false
        }
    }
}

// The codec the share goes in with watchers like these: HEVC unless one of
// them takes none.
fn codec_for(hevc: bool) -> Codec {
    if hevc { Codec::Hevc } else { Codec::H264 }
}

fn encoder_codec(numbers: &SharingNumbers) -> Codec {
    if numbers.encoder.contains("HEVC") {
        Codec::Hevc
    } else {
        Codec::H264
    }
}

fn speakers(config: &Config) -> Fake {
    match &config.voice.devices {
        Devices::Fake { speakers, .. } => speakers.clone(),
        Devices::Windows => panic!("no test here plays a sound"),
    }
}

fn room_of(host: Config, friends: Vec<Config>) -> (Member, Vec<Member>) {
    let host = Member::host_with(host);
    host.room().new_invite(true);
    let view = host.wait_for(WAIT, "a multi-use invite", |v| {
        v.invite
            .as_ref()
            .is_some_and(|i| i.multi_use && !i.code.is_empty())
    });
    let invite = code_to_invite(&view.invite.expect("an invite").code, loopback(host.port()));
    let people = friends.len() + 1;
    let friends: Vec<Member> = friends
        .into_iter()
        .map(|config| {
            Member::join_with(
                config,
                std::sync::Arc::new(keys::Identity::generate()),
                invite.clone(),
            )
        })
        .collect();
    for friend in &friends {
        friend.wait_for(WAIT, "live with everyone", |v| {
            v.strip.state == LinkState::Live && v.people.len() == people
        });
    }
    host.wait_for(WAIT, "everyone in", |v| v.people.len() == people);
    (host, friends)
}

fn own_share(view: &View) -> Option<u32> {
    match view.share.own {
        OwnShare::Sharing { number, .. } => Some(number),
        _ => None,
    }
}

fn shares(member: &Member, fps: u8) -> u32 {
    member.room().share(fps, None);
    let view = member.wait_for(WAIT, "the share running", |v| {
        own_share(v).is_some() && v.share.running.is_some()
    });
    own_share(&view).expect("granted")
}

fn watches(member: &Member, share: u32) {
    member.wait_for(WAIT, "the share in the roster", |v| {
        v.share.current.as_ref().is_some_and(|c| c.number == share)
    });
    member.room().watch(share, true);
    member.wait_for(WAIT, "the viewer open", |v| {
        v.share.watching && v.share.viewer_open
    });
}

fn sharing_numbers(member: &Member) -> SharingNumbers {
    member
        .view()
        .numbers
        .sharing
        .expect("numbers while sharing")
}

fn watching_numbers(member: &Member) -> WatchingNumbers {
    member
        .view()
        .numbers
        .watching
        .expect("numbers while watching")
}

fn next_second(member: &Member, after: &WatchingNumbers) -> WatchingNumbers {
    let shown = after.shown;
    member
        .wait_for(Duration::from_secs(3), "the next second's numbers", |v| {
            v.numbers
                .watching
                .as_ref()
                .is_some_and(|w| w.shown != shown)
        })
        .numbers
        .watching
        .expect("numbers while watching")
}

fn print_watched(who: &str, numbers: &WatchingNumbers) {
    let latency = numbers.capture_to_display.map_or_else(
        || String::from("not measured"),
        |l| {
            format!(
                "median {:.2} ms, p95 {:.2} ms{}",
                l.median_ms,
                l.p95_ms,
                if l.about { ", about" } else { "" }
            )
        },
    );
    println!(
        "{who}: {} fps in {:?}, capture to display over 10 s {latency}, decode on the GPU {}, loss {}, shown {}, repaired {}, dropped {}, not decoded {}, before the first IDR {}, {:?}",
        numbers.fps,
        numbers.codec,
        numbers
            .decode_ms
            .map_or_else(|| String::from("not measured"), |ms| format!("{ms:.2} ms")),
        numbers
            .video_loss_pct
            .map_or_else(|| String::from("not measured"), |pct| format!("{pct:.1}%")),
        numbers.shown,
        numbers.repaired,
        numbers.dropped,
        numbers.decode_failed,
        numbers.before_first_idr,
        numbers.present_path,
    );
}

fn print_shared(numbers: &SharingNumbers) {
    println!(
        "sharer: {} at {}x{} {} fps ({} encoded last second), encode {}, video {} kbit/s at a rate of {} of {} allowed ({} backoffs), parity {}%, {} IDRs ({} in the last minute), {} invalidations in the last minute, upload {} kbit/s, stepped down {:?}",
        numbers.encoder,
        numbers.width,
        numbers.height,
        numbers.fps,
        numbers.encoded_fps,
        numbers.encode_ms.map_or_else(
            || String::from("not measured"),
            |l| format!("median {:.2} ms, p95 {:.2} ms", l.median_ms, l.p95_ms)
        ),
        numbers.video_kbps,
        numbers.rate_kbps,
        numbers.allowed_kbps,
        numbers.backoffs,
        numbers.parity_pct,
        numbers.idrs,
        numbers.idrs_last_minute,
        numbers.invalidations_last_minute,
        numbers.upload_kbps,
        numbers.stepped_down,
    );
}

// Frames the send thread let go between two readings, and how many of them
// were IDRs, each of which was made again.
fn let_go_since(before: &SharingNumbers, after: &SharingNumbers) -> (u64, u64) {
    (
        after.let_go - before.let_go,
        after.idrs_let_go - before.idrs_let_go,
    )
}

fn lines(view: &View, kind: LineKind) -> Vec<String> {
    view.chat
        .iter()
        .filter(|line| line.kind == kind)
        .map(|line| line.text.clone())
        .collect()
}

fn timers_held_come_to(held: u32) {
    poll(WAIT, "the timer resolution given back", || {
        (share::fine_timers_held() == held).then_some(())
    });
}

#[test]
fn host_shares_and_friends_watch() {
    let _turn = turn();
    if !ready() {
        return;
    }
    let mara = config_with("Mara", pattern(None), room::DEFAULT_VIDEO_UPLOAD_KBPS);
    let mara_speakers = speakers(&mara);
    let (host, friends) = room_of(
        mara,
        vec![
            config_with("Ana", pattern(None), room::DEFAULT_VIDEO_UPLOAD_KBPS),
            config_with("Bo", pattern(None), room::DEFAULT_VIDEO_UPLOAD_KBPS),
        ],
    );
    let (ana, bo) = (&friends[0], &friends[1]);
    assert_eq!(share::fine_timers_held(), 0);
    assert!(host.view().numbers.sharing.is_none());
    assert!(ana.view().numbers.watching.is_none());

    let number = shares(&host, 120);
    assert_eq!(share::fine_timers_held(), 1, "1 ms timer while sharing");
    // The share's thread opened the capture and the encoder: the rising cue.
    let cues = heard_cues(&mara_speakers, 1);
    assert!(cues.len() == 1 && cues[0].rising, "{cues:?}");
    watches(ana, number);
    // Four seconds of frames: the numbers are the last one's, and capture to
    // display the last ten's.
    let first = poll(WAIT, "Ana's first numbers", || ana.view().numbers.watching);
    let mut seconds = vec![first];
    for _ in 0..4 {
        let next = next_second(ana, seconds.last().expect("a second"));
        seconds.push(next);
    }
    let watched = seconds.last().expect("a second").clone();
    print_watched("Ana", &watched);
    let shared = sharing_numbers(&host);
    print_shared(&shared);
    assert!(watched.fps >= 100, "{} frames a second of 120", watched.fps);
    let latency = watched.capture_to_display.expect("capture to display");
    assert!(
        latency.median_ms > 0.0 && latency.median_ms < 50.0,
        "{latency:?}"
    );
    assert!(watched.decode_ms.is_some());
    // Every frame shown decoded, so the first one was an IDR: a frame that
    // needs another could not have.
    assert_eq!(watched.decode_failed, 0);
    // Nothing is lost on loopback, but a debug build seals slowly, and a
    // frame the pacer has not started when the next one comes is let go.
    // Fewer than 1 in 100 dropped holds all the same.
    assert!(
        watched.dropped * 100 < watched.shown,
        "{} dropped of {}",
        watched.dropped,
        watched.shown
    );
    assert_eq!(shared.parity_pct, 20);
    assert_eq!((shared.width, shared.height, shared.fps), (2560, 1440, 120));
    assert!(shared.encoder.starts_with("NVENC"), "{}", shared.encoder);
    // HEVC wherever this PC's GPU decodes it, as every watcher here does;
    // the watcher shows what the encoder makes either way.
    assert_eq!(watched.codec, Some(encoder_codec(&shared)));
    if share::primary_takes_hevc().is_ok() {
        assert_eq!(encoder_codec(&shared), Codec::Hevc);
    }
    assert_eq!(shared.rate_kbps, room::DEFAULT_VIDEO_UPLOAD_KBPS);
    assert!(shared.upload_kbps >= shared.video_kbps, "{shared:?}");

    // A second watcher: one IDR for it, and no more. An IDR the send thread
    // let go is made again at once, so each one let go meanwhile adds one.
    let before = sharing_numbers(&host);
    watches(bo, number);
    poll(WAIT, "Bo's frames", || {
        bo.view()
            .numbers
            .watching
            .filter(|numbers| numbers.shown > 0)
    });
    thread::sleep(Duration::from_millis(2500));
    let after = sharing_numbers(&host);
    let (let_go, idrs_let_go) = let_go_since(&before, &after);
    println!(
        "IDRs before Bo watched {}, after {}; frames the send thread let go meanwhile {let_go}, {idrs_let_go} of them IDRs",
        before.idrs, after.idrs
    );
    assert!(
        after.idrs > before.idrs && after.idrs <= before.idrs + 1 + idrs_let_go,
        "{} IDRs, then {}, with {let_go} frames let go, {idrs_let_go} of them IDRs",
        before.idrs,
        after.idrs
    );
    assert_eq!(watching_numbers(bo).decode_failed, 0);

    // Bo closes the viewer: it stops watching, and the host stops sending
    // the share to Bo.
    bo.room().close_viewer();
    bo.wait_for(WAIT, "Bo stopped watching", |v| {
        !v.share.watching && !v.share.viewer_open && v.numbers.watching.is_none()
    });
    host.wait_for(WAIT, "one watcher left", |v| {
        v.share.current.as_ref().and_then(|c| c.watchers) == Some(1)
    });
    thread::sleep(Duration::from_millis(300));
    let received = bo.view().numbers.bytes_received;
    thread::sleep(Duration::from_secs(1));
    let quiet = bo.view().numbers.bytes_received - received;
    let flowing = {
        let before = ana.view().numbers.bytes_received;
        thread::sleep(Duration::from_secs(1));
        ana.view().numbers.bytes_received - before
    };
    println!(
        "in a second after closing, Bo received {quiet} bytes; Ana, still watching, {flowing}"
    );
    assert!(quiet * 20 < flowing, "{quiet} against {flowing}");

    // The share ends: Ana's viewer closes and the chat says so.
    host.room().stop_sharing();
    let view = ana.wait_for(WAIT, "Ana's viewer closed", |v| {
        !v.share.viewer_open && !v.share.watching && v.numbers.watching.is_none()
    });
    assert!(
        lines(&view, LineKind::System).contains(&String::from("Mara stopped sharing")),
        "{:?}",
        lines(&view, LineKind::System)
    );
    let view = host.wait_for(WAIT, "the host's share over", |v| {
        v.share.running.is_none() && v.numbers.sharing.is_none()
    });
    assert_eq!(view.share.own, OwnShare::Off);
    assert!(lines(&view, LineKind::Problem).is_empty());
    timers_held_come_to(0);
    // One falling cue, and the thread's closing after it plays no other.
    thread::sleep(Duration::from_millis(300));
    let cues = heard_cues(&mara_speakers, 2);
    assert!(cues.len() == 2 && !cues[1].rising, "{cues:?}");
}

#[test]
fn friends_share_through_the_host() {
    let _turn = turn();
    if !ready() {
        return;
    }
    let (host, friends) = room_of(
        config_with("Mara", pattern(None), room::DEFAULT_VIDEO_UPLOAD_KBPS),
        vec![
            config_with("Ana", pattern(None), room::DEFAULT_VIDEO_UPLOAD_KBPS),
            config_with("Bo", pattern(None), room::DEFAULT_VIDEO_UPLOAD_KBPS),
        ],
    );
    let (ana, bo) = (&friends[0], &friends[1]);
    let number = shares(ana, 120);
    watches(&host, number);
    watches(bo, number);
    let host_first = poll(WAIT, "the host's numbers", || host.view().numbers.watching);
    let bo_first = poll(WAIT, "Bo's numbers", || bo.view().numbers.watching);
    let (mut on_host, mut on_bo) = (host_first, bo_first);
    for _ in 0..3 {
        on_host = next_second(&host, &on_host);
        on_bo = next_second(bo, &on_bo);
    }
    print_watched("the host", &on_host);
    print_watched("Bo", &on_bo);
    print_shared(&sharing_numbers(ana));
    for (who, numbers) in [("the host", &on_host), ("Bo", &on_bo)] {
        assert!(numbers.fps >= 100, "{who}: {} fps", numbers.fps);
        assert!(numbers.capture_to_display.is_some(), "{who}");
        assert_eq!(numbers.decode_failed, 0, "{who}");
    }
    let relayed = host.view().numbers.video_relayed;
    assert!(relayed > 300, "the host passed on {relayed} packets");

    // Bo closes the viewer: the host stops passing the share on to Bo, the
    // one it relayed to, and goes on showing it itself.
    bo.room().close_viewer();
    bo.wait_for(WAIT, "Bo stopped watching", |v| {
        !v.share.watching && !v.share.viewer_open
    });
    host.wait_for(WAIT, "only the host watching", |v| {
        v.share.current.as_ref().and_then(|c| c.watchers) == Some(1)
    });
    thread::sleep(Duration::from_millis(300));
    let before = host.view().numbers.video_relayed;
    let shown = watching_numbers(&host).shown;
    thread::sleep(Duration::from_secs(1));
    let view = host.view();
    let still = view
        .numbers
        .watching
        .expect("the host still watching")
        .shown;
    println!(
        "a second after Bo closed the viewer the host relayed {} packets and showed {} frames",
        view.numbers.video_relayed - before,
        still - shown
    );
    assert_eq!(view.numbers.video_relayed, before);
    assert!(still - shown > 60);

    ana.room().stop_sharing();
    for (who, member) in [("the host", &host), ("Bo", bo)] {
        let view = member.wait_for(WAIT, "the viewer closed", |v| {
            !v.share.viewer_open && v.share.current.is_none()
        });
        assert!(
            lines(&view, LineKind::System).contains(&String::from("Ana stopped sharing")),
            "{who}: {:?}",
            lines(&view, LineKind::System)
        );
    }
    timers_held_come_to(0);
}

// A share goes in HEVC while every watcher's GPU decodes it. A watcher that
// says it does not switches it to H.264 with one IDR, which is also the first
// frame that watcher needs, and it goes back to HEVC, with one IDR again, a
// SWITCH_GAP after that watcher stopped, and to H.264 again when that watcher
// comes back, a SWITCH_GAP after the last change. The watcher who stays sees
// every frame in both codecs: its viewer makes a new decoder at each IDR that
// starts one.
#[test]
fn watcher_without_hevc_switches_to_h264_and_back() {
    let _turn = turn();
    if !ready() || !takes_hevc() {
        return;
    }
    let upload = room::DEFAULT_VIDEO_UPLOAD_KBPS;
    let (host, friends) = room_of(
        config_with("Mara", pattern(None), upload),
        vec![
            config_with("Ana", pattern(None), upload),
            without_hevc(config_with("Bo", pattern(None), upload)),
        ],
    );
    let (ana, bo) = (&friends[0], &friends[1]);
    let number = shares(&host, 120);
    watches(ana, number);
    let in_codec = |member: &Member, codec: Codec, what: &str| {
        poll(WAIT + share::SWITCH_GAP, what, || {
            member
                .view()
                .numbers
                .watching
                .filter(|numbers| numbers.codec == Some(codec) && numbers.fps > 0)
        })
    };
    let sharing_in = |codec: Codec, what: &str| {
        poll(WAIT + share::SWITCH_GAP, what, || {
            host.view()
                .numbers
                .sharing
                .filter(|numbers| encoder_codec(numbers) == codec)
        })
    };
    in_codec(ana, Codec::Hevc, "Ana watching in HEVC");
    let before = sharing_in(Codec::Hevc, "the share in HEVC");
    print_shared(&before);

    watches(bo, number);
    let switched = sharing_in(Codec::H264, "the share in H.264 for Bo");
    let on_bo = in_codec(bo, Codec::H264, "Bo watching in H.264");
    let on_ana = in_codec(ana, Codec::H264, "Ana watching in H.264");
    thread::sleep(Duration::from_millis(1500));
    let after = sharing_numbers(&host);
    print_shared(&after);
    print_watched("Bo", &on_bo);
    print_watched("Ana", &on_ana);
    let (let_go, idrs_let_go) = let_go_since(&before, &after);
    println!(
        "IDRs before Bo watched {}, after the switch {}; frames the send thread let go meanwhile {let_go}, {idrs_let_go} of them IDRs; {} IDRs a second after it",
        before.idrs, switched.idrs, after.idrs
    );
    assert_eq!(encoder_codec(&after), Codec::H264);
    // One IDR for Bo's start and the switch both: an HEVC IDR for his ask
    // and then an H.264 one would be two.
    assert!(
        after.idrs > before.idrs && after.idrs <= before.idrs + 1 + idrs_let_go,
        "{} IDRs, then {}, with {let_go} frames let go, {idrs_let_go} of them IDRs",
        before.idrs,
        after.idrs
    );
    for (who, member) in [("Ana", ana), ("Bo", bo)] {
        let numbers = watching_numbers(member);
        assert_eq!(numbers.decode_failed, 0, "{who}");
        assert_eq!(numbers.codec, Some(Codec::H264), "{who}");
    }

    // Bo stops watching: back to HEVC a gap later.
    let left = Instant::now();
    bo.room().watch(number, false);
    let back = sharing_in(Codec::Hevc, "the share in HEVC again");
    let took = left.elapsed();
    let on_ana = in_codec(ana, Codec::Hevc, "Ana watching in HEVC again");
    thread::sleep(Duration::from_millis(1500));
    let last = sharing_numbers(&host);
    print_shared(&last);
    print_watched("Ana", &on_ana);
    let (let_go, idrs_let_go) = let_go_since(&after, &last);
    println!(
        "HEVC again {took:?} after Bo stopped watching; IDRs {} then {}, frames let go meanwhile {let_go}, {idrs_let_go} of them IDRs",
        after.idrs, last.idrs
    );
    assert!(took >= share::SWITCH_GAP, "{took:?}");
    assert!(
        last.idrs > after.idrs && last.idrs <= after.idrs + 1 + idrs_let_go,
        "{} IDRs, then {}, with {let_go} frames let go, {idrs_let_go} of them IDRs",
        after.idrs,
        last.idrs
    );
    assert_eq!(encoder_codec(&back), Codec::Hevc);
    assert_eq!(watching_numbers(ana).decode_failed, 0);

    // Bo watches again within a SWITCH_GAP of that change: the share stays
    // in HEVC until the gap is over. His start costs an IDR in HEVC, which
    // his viewer drops, and the change to H.264 another, but his viewer
    // asks for none of the HEVC frames meanwhile: each ask would be one
    // more HEVC IDR he cannot use.
    let again = Instant::now();
    watches(bo, number);
    let h264_again = sharing_in(Codec::H264, "the share in H.264 for Bo again");
    let waited = again.elapsed();
    let on_bo = in_codec(bo, Codec::H264, "Bo watching in H.264 again");
    print_shared(&h264_again);
    print_watched("Bo", &on_bo);
    let (let_go, idrs_let_go) = let_go_since(&last, &h264_again);
    println!(
        "H.264 again {waited:?} after Bo watched again; IDRs {} then {}, frames let go meanwhile {let_go}, {idrs_let_go} of them IDRs",
        last.idrs, h264_again.idrs
    );
    assert!(
        h264_again.idrs > last.idrs && h264_again.idrs <= last.idrs + 2 + idrs_let_go,
        "{} IDRs, then {}, with {let_go} frames let go, {idrs_let_go} of them IDRs",
        last.idrs,
        h264_again.idrs
    );
    assert_eq!(watching_numbers(ana).decode_failed, 0);
    host.room().stop_sharing();
    ana.wait_for(WAIT, "the viewer closed", |v| !v.share.viewer_open);
    timers_held_come_to(0);
}

// The sharer's GPU encoder fails during a 120 fps share (encode::fault makes
// NVENC report an error for a frame): the share goes on with Windows'
// software encoder at 60 fps, the sharer's panel says so and its stats
// panel has the encoder in warn, the watcher's roster gets the new rate,
// and its viewer goes on in H.264, as with any codec change. The host's
// share, and a friend's, whose rate reaches the roster through the host.
//
// At 720p and 2 Mbit/s: the software encoder's IDRs can take 0.375 s of the
// rate, and a debug build seals one of 500 KB, its 1080p IDR at 15 Mbit/s,
// so slowly that the send thread lets the next frames go, and each costs the
// watcher another IDR. A release build sent that share whole at 60 fps.
#[test]
fn gpu_encoder_failure_goes_on_in_software() {
    let _turn = turn();
    if !ready() || !takes_hevc() {
        return;
    }
    let small = || VideoConfig {
        source: VideoSource::Pattern {
            width: 1280,
            height: 720,
            busy: false,
        },
        ..pattern(None)
    };
    let (host, friends) = room_of(
        config_with("Mara", small(), 2000),
        vec![config_with("Ana", small(), 2000)],
    );
    let ana = &friends[0];
    for (sharer, watcher) in [(&host, ana), (ana, &host)] {
        gpu_encoder_fails(sharer, watcher);
    }
    timers_held_come_to(0);
}

fn gpu_encoder_fails(sharer: &Member, watcher: &Member) {
    let gpu_failed = share::Software::GpuFailed.sentence();
    // About two seconds of watched frames on NVENC first. The share's
    // encoder is the next NVENC to open, and takes the fault as it opens.
    encode::fault::fail_nvenc(240);
    let number = shares(sharer, 120);
    let on_nvenc = poll(WAIT, "the sharer's first numbers", || {
        sharer.view().numbers.sharing
    });
    print_shared(&on_nvenc);
    assert!(
        on_nvenc.encoder.starts_with("NVENC") && !on_nvenc.software,
        "{on_nvenc:?}"
    );
    watches(watcher, number);

    let view = sharer.wait_for(WAIT, "the sharer's panel saying why", |v| {
        v.share.problem.is_some()
    });
    assert_eq!(view.share.problem.as_deref(), Some(gpu_failed));
    assert_eq!(lines(&view, LineKind::Problem), [gpu_failed]);
    watcher.wait_for(WAIT, "the roster at 60 fps", |v| {
        v.share
            .current
            .as_ref()
            .is_some_and(|current| current.number == number && current.fps == 60)
    });
    let first = poll(WAIT, "the watcher's frames in H.264", || {
        watcher
            .view()
            .numbers
            .watching
            .filter(|numbers| numbers.codec == Some(Codec::H264) && numbers.fps > 0)
    });
    let mut watched = first;
    for _ in 0..2 {
        watched = next_second(watcher, &watched);
    }
    let shared = sharing_numbers(sharer);
    print_shared(&shared);
    print_watched("the watcher", &watched);
    assert!(
        shared.software && shared.encoder.contains("software"),
        "{shared:?}"
    );
    assert_eq!((shared.width, shared.height, shared.fps), (1280, 720, 60));
    // A debug build seals the software encoder's large IDRs more slowly than
    // a frame interval, so now and then a frame is let go and costs another
    // IDR, and that second drops to about 35 fps. A release build holds 59
    // to 61.
    let floor = if cfg!(debug_assertions) { 20 } else { 50 };
    assert!(
        watched.fps >= floor,
        "{} frames a second of 60",
        watched.fps
    );
    assert_eq!(watched.decode_failed, 0);

    sharer.room().stop_sharing();
    watcher.wait_for(WAIT, "the viewer closed", |v| !v.share.viewer_open);
    let view = sharer.wait_for(WAIT, "the share over", |v| {
        v.share.running.is_none() && v.numbers.sharing.is_none()
    });
    // It never stopped on an error.
    assert_eq!(lines(&view, LineKind::Problem), [gpu_failed]);
    encode::fault::clear();
}

// The target for loss, with the knob on the watcher's side: at 5 percent
// fewer than 1 frame in 100 is lost for good, with the percentage at its
// floor and each frame's parity from the loss. In HEVC, which the share goes
// in when the watcher takes it, and in H.264.
#[test]
fn loss_at_5_percent() {
    let _turn = turn();
    if !ready() {
        return;
    }
    at_5_percent(false);
    if takes_hevc() {
        at_5_percent(true);
    }
}

fn at_5_percent(hevc: bool) {
    let knob = LossKnob {
        percent: 5.0,
        seed: 5,
    };
    let mut ana = config_with("Ana", pattern(Some(knob)), room::DEFAULT_VIDEO_UPLOAD_KBPS);
    ana.video.hevc = hevc;
    let (host, friends) = room_of(
        config_with("Mara", pattern(None), room::DEFAULT_VIDEO_UPLOAD_KBPS),
        vec![ana],
    );
    let ana = &friends[0];
    let number = shares(&host, 120);
    watches(ana, number);
    let mut numbers = poll(WAIT, "Ana's numbers", || ana.view().numbers.watching);
    for _ in 0..9 {
        numbers = next_second(ana, &numbers);
    }
    print_watched("Ana at 5% loss", &numbers);
    let shared = sharing_numbers(&host);
    print_shared(&shared);
    assert_eq!(numbers.codec, Some(codec_for(hevc)));
    assert_eq!(encoder_codec(&shared), codec_for(hevc));
    let knobbed = numbers.knob.expect("the knob's numbers");
    println!(
        "the knob dropped {} packets at {}% (seed {})",
        knobbed.dropped, knobbed.percent, knobbed.seed
    );
    assert!(knobbed.dropped > 0);
    let loss = numbers.video_loss_pct.expect("a loss number");
    assert!(loss > 2.0 && loss < 10.0, "{loss}%");
    assert_eq!(shared.parity_pct, 20, "twice 5 percent is under the floor");
    let frames = numbers.shown + numbers.dropped;
    assert!(frames > 800, "{frames} frames");
    // The same target in either codec: each frame's parity follows the loss
    // the watcher reports, so a frame of any size is lost for good under 1
    // time in 100. On this mostly still pattern NVENC's HEVC frames come out
    // at three to five data shards, which the floor alone gave one parity
    // shard and 5 percent beat 1.4 to 3.3 times in 100; H.264's one or two
    // shards were already under.
    let bytes = f64::from(shared.video_kbps) * 125.0 / f64::from(shared.encoded_fps.max(1));
    let data = data_shards_on_the_lan(bytes);
    let parity = channels::video::parity_for_loss(data, Some(loss));
    println!(
        "frames of {bytes:.0} bytes, {data} data shards, get {parity} parity shards at {loss:.1} \
         percent and lose {:.2} in 100 for good; {} of {frames} were",
        channels::video::lost_for_good(data, parity, f64::from(loss) / 100.0) * 100.0,
        numbers.dropped
    );
    assert!(
        numbers.dropped * 100 < frames,
        "{} dropped of {frames}",
        numbers.dropped
    );
    assert_eq!(numbers.decode_failed, 0);
    host.room().stop_sharing();
    ana.wait_for(WAIT, "the viewer closed", |v| !v.share.viewer_open);
    timers_held_come_to(0);
}

// Data shards for a frame of `bytes` on the LAN, at the largest shard.
fn data_shards_on_the_lan(bytes: f64) -> u16 {
    let shard = (share::PAYLOAD_LAN - channels::video::HEADER) / channels::video::SHARD_STEP
        * channels::video::SHARD_STEP;
    ((bytes + channels::video::FRAME_HEADER as f64) / shard as f64)
        .ceil()
        .max(1.0) as u16
}

// At 20 percent the parity follows the loss, lost frames are invalidated so
// the picture keeps moving, and the backoff lowers the rate the encoder is
// given. The host shares the busy pattern, which sends as much as its rate
// lets it: the plain one sends a fraction of any rate, where loss is never
// a queue (share::rate, NEAR_SHARE). The upload setting is low, so the
// backoff goes a long way down within the run.
// In HEVC and in H.264, as at 5 percent.
#[test]
fn loss_at_20_percent() {
    let _turn = turn();
    if !ready() {
        return;
    }
    at_20_percent(false);
    if takes_hevc() {
        at_20_percent(true);
    }
}

fn at_20_percent(hevc: bool) {
    let knob = LossKnob {
        percent: 20.0,
        seed: 20,
    };
    let upload = 2_000;
    let mut ana = config_with("Ana", pattern(Some(knob)), room::DEFAULT_VIDEO_UPLOAD_KBPS);
    ana.video.hevc = hevc;
    let (host, friends) = room_of(config_with("Mara", busy(), upload), vec![ana]);
    let ana = &friends[0];
    let number = shares(&host, 120);
    watches(ana, number);
    let mut numbers = poll(WAIT, "Ana's numbers", || ana.view().numbers.watching);
    let mut fps = Vec::new();
    for _ in 0..9 {
        numbers = next_second(ana, &numbers);
        fps.push(numbers.fps);
    }
    print_watched("Ana at 20% loss", &numbers);
    println!("frames shown each second: {fps:?}");
    let shared = sharing_numbers(&host);
    print_shared(&shared);
    assert_eq!(numbers.codec, Some(codec_for(hevc)));
    assert_eq!(encoder_codec(&shared), codec_for(hevc));
    assert!(shared.parity_pct > 30, "parity {}%", shared.parity_pct);
    assert!(
        shared.invalidations_last_minute > 0,
        "recover ranges reached the encoder"
    );
    // After the first second the picture keeps moving.
    let slowest = fps[1..].iter().min().copied().unwrap_or(0);
    assert!(slowest >= 60, "{fps:?}");
    assert!(
        shared.backoffs > 0 && shared.rate_kbps < shared.allowed_kbps,
        "{shared:?}"
    );
    assert_eq!(shared.allowed_kbps, upload);
    // NVENC took each lower rate.
    assert_eq!(shared.encoder_kbps, shared.rate_kbps, "{shared:?}");
    host.room().stop_sharing();
    ana.wait_for(WAIT, "the viewer closed", |v| !v.share.viewer_open);
    timers_held_come_to(0);
}

// Nothing is captured here, nor could be: the source is the test
// pattern, at a size the room refuses before anything opens, so no change
// to how a monitor is chosen can bring the screen into it. The timer
// resolution raised for the share is given back all the same, and a
// friend's failed share is ended at the host. The sentence for a monitor
// that is gone is screen::sharer's unit test.
#[test]
fn share_that_cannot_start_says_why() {
    let _turn = turn();
    let nothing = VideoConfig {
        source: VideoSource::Pattern {
            width: 0,
            height: 0,
            busy: false,
        },
        show: Show::NoActivate,
        ..VideoConfig::default()
    };
    let (host, friends) = room_of(
        config_with("Mara", nothing.clone(), room::DEFAULT_VIDEO_UPLOAD_KBPS),
        vec![config_with("Ana", nothing, room::DEFAULT_VIDEO_UPLOAD_KBPS)],
    );
    let ana = &friends[0];
    for (who, member) in [("the host", &host), ("Ana", ana)] {
        member.room().share(60, None);
        let view = member.wait_for(WAIT, "the problem line", |v| {
            !lines(v, LineKind::Problem).is_empty() && v.share.own == OwnShare::Off
        });
        let problems = lines(&view, LineKind::Problem);
        println!("{who}: {problems:?}");
        assert_eq!(
            problems,
            ["Could not start sharing: the test pattern cannot be 0x0."]
        );
        assert_eq!(view.share.problem.as_deref(), Some(problems[0].as_str()));
        assert!(view.share.running.is_none() && view.numbers.sharing.is_none());
        assert_eq!(view.numbers.video_sent, 0, "{who}");
        timers_held_come_to(0);
        // Everyone is told the share is over.
        host.wait_for(WAIT, "no share in the room", |v| v.share.current.is_none());
        ana.wait_for(WAIT, "no share in the room", |v| v.share.current.is_none());
    }
}
