// That PC sound leaves Booth out. Four quiet tones play on an output that is
// not Windows' default where there is one, while PC sound captures: 1500 and
// 1900 Hz from this process, which it must
// not hear, and 700 and 1100 Hz from a second copy of this test program
// started under the shell, outside this process's tree, which it must hear.
// One of each starts before the capture opens and one after, as a room's
// speakers open before PC sound or again after a headset drops. This
// process's tones go on and off, so a video or a game playing meanwhile
// counts for nothing; the other's play steadily, loud enough to stand over
// one. Of what is captured only the level at those four frequencies is kept.
//
// It plays sound, so it runs only when asked:
//
//   cargo test -p voice --test audio_pc_sound -- --ignored --nocapture

use std::f64::consts::PI;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use voice::audio::{self, Choice, Direction, Event, PC_SOUND_BUILD, PcSound, Render};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetExitCodeProcess, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    OpenProcess, PROC_THREAD_ATTRIBUTE_PARENT_PROCESS, PROCESS_CREATE_PROCESS, PROCESS_INFORMATION,
    PROCESS_SYNCHRONIZE, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId};
use windows::core::{PCWSTR, PWSTR};

const RATE: f64 = 48_000.0;
// -30 dBFS, on every channel.
const LOUDNESS: f32 = 0.031_6;
const INSIDE: [f64; 2] = [1500.0, 1900.0];
const OUTSIDE: [f64; 2] = [700.0, 1100.0];
// 20 ms, so every tone falls on a bin of its own, 50 Hz apart.
const BLOCK: usize = 960;
const BLOCK_TIME: Duration = Duration::from_millis(20);
const FLOOR_DB: f64 = -90.0;
// This process's tones are on this long and then off as long, so their on
// blocks can be told from the sound of whatever else plays meanwhile, which
// the off blocks hold. Blocks this near an edge are left out, for the render
// queue and the engine's pass.
const GATE: Duration = Duration::from_millis(250);
const GUARD: Duration = Duration::from_millis(60);
// Every tone plays at -30 dBFS, and the other process's come through at
// that to a tenth of a dB, so a tone of this process's let through would
// too. Heard, a tone stands over this; left out, its on blocks stay under
// the other, where a video playing at the same frequencies sits too (-50 to
// -70 dBFS here).
const HEARD_DB: f64 = -40.0;
const LEFT_OUT_DB: f64 = -36.0;
const PHASE: Duration = Duration::from_millis(1500);
// How long the other process may live, whatever happens to this one.
const OTHER_LIVES: Duration = Duration::from_secs(15);
const FOLDER: &str = "booth-pc-sound-test";

fn folder() -> PathBuf {
    std::env::temp_dir().join(FOLDER)
}

fn mark(folder: &Path, name: &str) {
    fs::write(folder.join(name), "").expect("write a mark for the other process");
}

// True once `name` is in the folder; false at `until`.
fn wait_mark(folder: &Path, name: &str, until: Instant) -> bool {
    while Instant::now() < until {
        if folder.join(name).exists() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

// Where the tones play: an output that is not Windows' default where there
// is one, so a person listening on the default hears nothing; PC sound takes
// every program's sound whatever device it plays to.
fn tone_output() -> Choice {
    let outputs = audio::lists().expect("list the devices").outputs;
    outputs
        .devices
        .iter()
        .find(|device| !device.is_default)
        .map_or(Choice::Default, |device| Choice::Device(device.id.clone()))
}

// A sine for as long as it lives, through Booth's own render thread; gated,
// on for GATE and off as long, counted from its first sample.
struct Tone {
    _render: Render,
    first: Arc<Mutex<Option<Instant>>>,
}

impl Tone {
    fn start(hz: f64, gated: bool) -> Tone {
        let first = Arc::new(Mutex::new(None));
        let started = Arc::clone(&first);
        let gate = (GATE.as_secs_f64() * RATE) as u64;
        let mut n = 0u64;
        let (tx, rx) = mpsc::channel();
        let render = Render::start(
            tone_output(),
            move |samples: &mut [f32]| {
                started.lock().unwrap().get_or_insert_with(Instant::now);
                for sample in samples.iter_mut() {
                    let on = !gated || (n / gate).is_multiple_of(2);
                    let wave = (2.0 * PI * hz * n as f64 / RATE).sin() as f32;
                    *sample = if on { LOUDNESS * wave } else { 0.0 };
                    n += 1;
                }
            },
            move |event| {
                let _ = tx.send(event);
            },
        )
        .expect("start a tone");
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Event::Opened(_)) => Tone {
                _render: render,
                first,
            },
            other => panic!("the {hz} Hz tone did not open: {other:?}"),
        }
    }

    // On, off, or near an edge, for a block from `at`.
    fn on_at(&self, at: Instant) -> Option<bool> {
        let first = (*self.first.lock().unwrap())?;
        let since = at.checked_duration_since(first)?;
        let into = since.as_secs_f64() % (2.0 * GATE.as_secs_f64());
        let (from, to) = (into, into + BLOCK_TIME.as_secs_f64());
        let (gate, guard) = (GATE.as_secs_f64(), GUARD.as_secs_f64());
        if from >= guard && to <= gate - guard {
            Some(true)
        } else if from >= gate + guard && to <= 2.0 * gate - guard {
            Some(false)
        } else {
            None
        }
    }
}

// The level of a block at `hz`, in dBFS, through a Blackman window.
fn level_db(block: &[f32], hz: f64) -> f64 {
    let n = block.len() as f64;
    let coeff = 2.0 * (2.0 * PI * hz / RATE).cos();
    let (mut s1, mut s2, mut sum) = (0.0f64, 0.0f64, 0.0f64);
    for (i, &x) in block.iter().enumerate() {
        let t = i as f64 / (n - 1.0);
        let w = 0.42 - 0.5 * (2.0 * PI * t).cos() + 0.08 * (4.0 * PI * t).cos();
        sum += w;
        let s = f64::from(x) * w + coeff * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    let power = (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0);
    let amplitude = 2.0 * power.sqrt() / sum;
    (20.0 * amplitude.max(1e-12).log10()).max(FLOOR_DB)
}

type Heard = Arc<Mutex<Vec<(Instant, Vec<f32>)>>>;

// PC sound captured into `heard`, both channels' average, by packet time.
fn capture(heard: &Heard) -> (PcSound, Receiver<Event>) {
    let into = Arc::clone(heard);
    let (tx, rx) = mpsc::channel();
    let sound = PcSound::start(
        move |stereo: &[f32], time, _| {
            let (pairs, _) = stereo.as_chunks::<2>();
            let mid = pairs
                .iter()
                .map(|[left, right]| (left + right) * 0.5)
                .collect();
            into.lock().unwrap().push((time, mid));
        },
        move |event| {
            let _ = tx.send(event);
        },
    )
    .expect("start pc sound");
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Event::Opened(_)) => (sound, rx),
        other => panic!("pc sound did not open: {other:?}"),
    }
}

// What was captured from `from` to `to`, in blocks of BLOCK samples, each
// with the time of its first one.
fn blocks(heard: &Heard, from: Instant, to: Instant) -> Vec<(Instant, Vec<f32>)> {
    let mut blocks = Vec::new();
    let mut block: Vec<f32> = Vec::with_capacity(BLOCK);
    let mut start = from;
    for (time, mid) in heard.lock().unwrap().iter() {
        if *time < from || *time >= to {
            continue;
        }
        for (i, &sample) in mid.iter().enumerate() {
            if block.is_empty() {
                start = *time + Duration::from_secs_f64(i as f64 / RATE);
            }
            block.push(sample);
            if block.len() == BLOCK {
                blocks.push((start, std::mem::take(&mut block)));
            }
        }
    }
    assert!(blocks.len() >= 20, "only {} blocks captured", blocks.len());
    blocks
}

fn median(mut levels: Vec<f64>) -> f64 {
    levels.sort_by(f64::total_cmp);
    levels[levels.len() / 2]
}

fn level(blocks: &[(Instant, Vec<f32>)], hz: f64) -> f64 {
    median(
        blocks
            .iter()
            .map(|(_, block)| level_db(block, hz))
            .collect(),
    )
}

// The median levels of a gated tone's on blocks and of its off blocks.
fn on_and_off(blocks: &[(Instant, Vec<f32>)], hz: f64, tone: &Tone) -> (f64, f64) {
    let (mut on, mut off) = (Vec::new(), Vec::new());
    for (at, block) in blocks {
        match tone.on_at(*at) {
            Some(true) => on.push(level_db(block, hz)),
            Some(false) => off.push(level_db(block, hz)),
            None => {}
        }
    }
    assert!(
        on.len() >= 5 && off.len() >= 5,
        "{} on and {} off blocks at {hz} Hz",
        on.len(),
        off.len()
    );
    (median(on), median(off))
}

// What Windows has noted of this program's microphone use, for the report.
fn microphone_record() -> String {
    let exe = std::env::current_exe().unwrap();
    let key = format!(
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone\NonPackaged\{}",
        exe.display().to_string().replace('\\', "#")
    );
    let out = Command::new("reg").args(["query", &key]).output();
    match out {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|line| line.contains("LastUsedTime"))
            .map(str::trim)
            .collect::<Vec<_>>()
            .join("; "),
        _ => String::from("none"),
    }
}

fn windows_build() -> u32 {
    let out = Command::new("reg")
        .args([
            "query",
            r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion",
            "/v",
            "CurrentBuildNumber",
        ])
        .output()
        .expect("run reg");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .last()
        .and_then(|build| build.parse().ok())
        .expect("a build number from reg")
}

// The other process: this test program started as a child of the shell, so
// it is outside this process's tree, running only plays_tones_for_pc_sound.
// Ended by the guard if it is still there when the test ends.
struct Other(HANDLE);

impl Other {
    fn start() -> Other {
        // SAFETY: a plain query.
        let shell = unsafe { GetShellWindow() };
        let mut shell_pid = 0;
        // SAFETY: a plain query on the shell's window.
        unsafe { GetWindowThreadProcessId(shell, Some(&mut shell_pid)) };
        assert_ne!(shell_pid, 0, "no process for the shell window");
        // SAFETY: a plain open; the handle is closed below once the new
        // process has been made from it.
        let parent = unsafe { OpenProcess(PROCESS_CREATE_PROCESS, false, shell_pid) }
            .expect("open the shell to start a process under it");
        let mut size = 0;
        // SAFETY: asks only for the size of a list of one attribute; this
        // first call fails by design.
        let _ = unsafe { InitializeProcThreadAttributeList(None, 1, None, &mut size) };
        let mut list = vec![0u8; size];
        let attributes = LPPROC_THREAD_ATTRIBUTE_LIST(list.as_mut_ptr().cast());
        // SAFETY: `list` has the size asked for and outlives the attribute
        // list, which is deleted before it goes; `parent` stays open until
        // then too.
        unsafe {
            InitializeProcThreadAttributeList(Some(attributes), 1, None, &mut size)
                .expect("make the attribute list");
            UpdateProcThreadAttribute(
                attributes,
                0,
                PROC_THREAD_ATTRIBUTE_PARENT_PROCESS as usize,
                Some((&parent as *const HANDLE).cast()),
                size_of::<HANDLE>(),
                None,
                None,
            )
            .expect("name the shell as the parent");
        }
        let exe = std::env::current_exe().unwrap();
        let mut command: Vec<u16> = format!(
            "\"{}\" plays_tones_for_pc_sound --exact --ignored --nocapture",
            exe.display()
        )
        .encode_utf16()
        .chain([0])
        .collect();
        let app: Vec<u16> = exe.as_os_str().encode_wide_nul();
        let info = STARTUPINFOEXW {
            StartupInfo: windows::Win32::System::Threading::STARTUPINFOW {
                cb: size_of::<STARTUPINFOEXW>() as u32,
                ..Default::default()
            },
            lpAttributeList: attributes,
        };
        let mut process = PROCESS_INFORMATION::default();
        // SAFETY: NUL-terminated strings alive for the call, a writable
        // command line as CreateProcessW wants, the extended start-up info
        // with its live attribute list, no handles inherited, and this
        // process's own environment.
        let made = unsafe {
            CreateProcessW(
                PCWSTR(app.as_ptr()),
                Some(PWSTR(command.as_mut_ptr())),
                None,
                None,
                false,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW,
                None,
                None,
                &info.StartupInfo,
                &mut process,
            )
        };
        // SAFETY: made above, and nothing uses either after this.
        unsafe {
            DeleteProcThreadAttributeList(attributes);
            let _ = CloseHandle(parent);
        }
        made.expect("start the other process under the shell");
        // SAFETY: the thread's handle is not needed.
        let _ = unsafe { CloseHandle(process.hThread) };
        Other(process.hProcess)
    }

    // Its exit code, once it has ended within `within`.
    fn ended(&self, within: Duration) -> Option<u32> {
        let ms = within.as_millis() as u32;
        // SAFETY: a live process handle and a plain wait.
        if unsafe { WaitForSingleObject(self.0, ms) } != WAIT_OBJECT_0 {
            return None;
        }
        let mut code = 0;
        // SAFETY: as above, with a live out parameter.
        unsafe { GetExitCodeProcess(self.0, &mut code) }.ok()?;
        Some(code)
    }
}

impl Drop for Other {
    fn drop(&mut self) {
        // SAFETY: the handle is ours; a process that ended already refuses.
        unsafe {
            let _ = TerminateProcess(self.0, 1);
            let _ = CloseHandle(self.0);
        }
    }
}

trait WideNul {
    fn encode_wide_nul(&self) -> Vec<u16>;
}

impl WideNul for std::ffi::OsStr {
    fn encode_wide_nul(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        self.encode_wide().chain([0]).collect()
    }
}

#[test]
#[ignore = "plays quiet tones; run by hand"]
fn pc_sound_leaves_this_process_out() {
    if audio::default_id(Direction::Output)
        .expect("ask for the default output")
        .is_none()
    {
        println!(
            "skipped: Windows has no speakers or headphones turned on, so the tones have nowhere to play"
        );
        return;
    }
    let build = windows_build();
    if build < PC_SOUND_BUILD {
        println!(
            "skipped: PC sound needs Windows build {PC_SOUND_BUILD} or later, this PC runs build {build}"
        );
        return;
    }
    // SAFETY: a plain query.
    if unsafe { GetShellWindow() }.is_invalid() {
        println!("skipped: no Windows shell runs in this session to start the other process from");
        return;
    }
    println!("the tones play on {:?}", tone_output());
    let folder = folder();
    let _ = fs::remove_dir_all(&folder);
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("parent"), std::process::id().to_string()).unwrap();
    let microphone_before = microphone_record();

    let tone_a = Tone::start(INSIDE[0], true);
    let other = Other::start();
    let deadline = Instant::now() + Duration::from_secs(10);
    assert!(
        wait_mark(&folder, "c", deadline),
        "the other process did not start its first tone; it ended with {:?}",
        other.ended(Duration::ZERO)
    );
    let heard: Heard = Arc::new(Mutex::new(Vec::new()));
    let opening = Instant::now();
    let (sound, events) = capture(&heard);
    let opened_in = opening.elapsed();
    thread::sleep(Duration::from_millis(200));
    let first_from = Instant::now();
    thread::sleep(PHASE);
    let first_to = Instant::now();
    mark(&folder, "capturing");
    let deadline = Instant::now() + Duration::from_secs(5);
    assert!(
        wait_mark(&folder, "d", deadline),
        "the other process did not start its second tone"
    );
    let tone_b = Tone::start(INSIDE[1], true);
    thread::sleep(Duration::from_millis(200));
    let second_from = Instant::now();
    thread::sleep(PHASE);
    let second_to = Instant::now();
    mark(&folder, "done");
    let (event_reads, quiet_reads) = sound.reads();
    let glitches = sound.glitches();
    let ended = other.ended(Duration::from_secs(5));
    sound.stop();
    let microphone_after = microphone_record();
    let lost: Vec<Event> = events.try_iter().collect();
    let _ = fs::remove_dir_all(&folder);

    let first = blocks(&heard, first_from, first_to);
    let second = blocks(&heard, second_from, second_to);
    let a_first = on_and_off(&first, INSIDE[0], &tone_a);
    let a_second = on_and_off(&second, INSIDE[0], &tone_a);
    let b_second = on_and_off(&second, INSIDE[1], &tone_b);
    let c_first = level(&first, OUTSIDE[0]);
    let c_second = level(&second, OUTSIDE[0]);
    let d_second = level(&second, OUTSIDE[1]);
    drop((tone_a, tone_b));
    println!(
        "opened in {:.1} ms; {event_reads} reads on the event, {quiet_reads} after a quiet wait, {glitches} glitches, events since {lost:?}",
        opened_in.as_secs_f64() * 1000.0
    );
    println!(
        "this process's tones, on blocks and off blocks: 1500 Hz {:.1} and {:.1} dBFS with one tone each, then {:.1} and {:.1}, 1900 Hz {:.1} and {:.1}",
        a_first.0, a_first.1, a_second.0, a_second.1, b_second.0, b_second.1
    );
    println!(
        "the other process's steady tones: 700 Hz {c_first:.1} dBFS with one tone each, then {c_second:.1} dBFS, 1100 Hz {d_second:.1} dBFS"
    );
    println!(
        "microphone use as Windows records it: before {microphone_before}; after {microphone_after}"
    );
    assert_eq!(ended, Some(0), "the other process did not end well");
    for (what, db) in [
        ("the other process's tone from before the capture", c_first),
        ("the other process's tone from before the capture", c_second),
        (
            "the other process's tone from after the capture opened",
            d_second,
        ),
    ] {
        assert!(
            db > HEARD_DB,
            "{what} came through at {db:.1} dBFS; is the output muted, or its volume down?"
        );
    }
    for (what, (on, off)) in [
        ("this process's tone from before the capture", a_first),
        ("this process's tone from before the capture", a_second),
        (
            "this process's tone from after the capture opened",
            b_second,
        ),
    ] {
        assert!(
            on < LEFT_OUT_DB,
            "{what} came through at {on:.1} dBFS in its on blocks ({off:.1} in its off blocks); unless something else plays that loud there, PC sound took this process's own sound"
        );
    }
}

// The other process of pc_sound_leaves_this_process_out: does nothing unless
// that test made the folder, and ends by itself at OTHER_LIVES or as soon as
// that test's process ends.
#[test]
#[ignore = "the other process of pc_sound_leaves_this_process_out"]
fn plays_tones_for_pc_sound() {
    let folder = folder();
    let Some(parent) = fs::read_to_string(folder.join("parent"))
        .ok()
        .and_then(|pid| pid.trim().parse::<u32>().ok())
    else {
        return;
    };
    if parent == std::process::id() {
        return;
    }
    // SAFETY: a plain open of the test's process, to wait on it.
    let Ok(test) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, parent) }) else {
        return;
    };
    let until = Instant::now() + OTHER_LIVES;
    let test_alive = || {
        // SAFETY: a live handle and a wait that returns at once.
        let state = unsafe { WaitForSingleObject(test, 0) };
        state != WAIT_OBJECT_0
    };
    let tone_c = Tone::start(OUTSIDE[0], false);
    thread::sleep(Duration::from_millis(200));
    mark(&folder, "c");
    while !folder.join("capturing").exists() {
        if Instant::now() >= until || !test_alive() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let tone_d = Tone::start(OUTSIDE[1], false);
    thread::sleep(Duration::from_millis(200));
    mark(&folder, "d");
    while !folder.join("done").exists() && Instant::now() < until && test_alive() {
        thread::sleep(Duration::from_millis(10));
    }
    drop((tone_c, tone_d));
    // SAFETY: opened above.
    let _ = unsafe { CloseHandle(test) };
}
