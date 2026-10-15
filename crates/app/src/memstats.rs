// The memstats build, for finding what keeps growing in a long session.
// Once a minute memstats.csv in the profile folder gets the process's
// working set and private bytes, every gauge the crates set (stats::mem),
// the heap Booth's own code holds by the name of the thread that took it,
// and the sizes of egui's caches. --fake-audio swaps the microphone for a
// tone that comes and goes and the speakers for a sink, so voice runs on a
// PC with no microphone.
//
// The heap counts come from a global allocator that puts a small header in
// front of every block with the thread it was taken on, so a block freed on
// another thread is still taken off the right count.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::Context;
use room::Devices;
use voice::audio::fake::{Fake, Setup};
use windows_sys::Win32::Foundation::{HLOCAL, LocalFree};
use windows_sys::Win32::System::ProcessStatus::{
    K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows_sys::Win32::System::Threading::{
    GR_GDIOBJECTS, GR_USEROBJECTS, GetCurrentProcess, GetCurrentThread, GetGuiResources,
    GetProcessHandleCount, GetThreadDescription,
};

const EVERY: Duration = Duration::from_secs(60);
const EGUI_EVERY: Duration = Duration::from_secs(5);
const FAKE_AUDIO: &str = "--fake-audio";

static FAKE: AtomicBool = AtomicBool::new(false);

// Takes out what only this build knows before the usual parsing sees it.
pub fn take_flags(args: Vec<OsString>) -> Vec<OsString> {
    let before = args.len();
    let args: Vec<OsString> = args.into_iter().filter(|arg| arg != FAKE_AUDIO).collect();
    FAKE.store(args.len() != before, Ordering::Relaxed);
    args
}

// None for Windows' own devices, which is what a run without --fake-audio
// gets.
pub fn devices() -> Option<Devices> {
    static PAIR: OnceLock<(Fake, Fake)> = OnceLock::new();
    if !FAKE.load(Ordering::Relaxed) {
        return None;
    }
    let (microphone, speakers) = PAIR.get_or_init(|| {
        let tone = Setup {
            signal: tone,
            ..Setup::default()
        };
        (
            Fake::unrecorded(tone, &[("tone", "Tone")], Some("tone")),
            Fake::unrecorded(Setup::default(), &[("sink", "Sink")], Some("sink")),
        )
    });
    Some(Devices::Fake {
        microphone: microphone.clone(),
        speakers: speakers.clone(),
    })
}

// 440 Hz at 48 kHz, 2.5 s on and 0.5 s off: open mic takes the quietest
// block of the last second as the room's noise, so a tone that never stops
// would never count as a voice. A whole number of cycles a second, so the
// phase never jumps where the frame count wraps.
fn tone(frame: u64, _channel: u16) -> f32 {
    if frame % 144_000 >= 120_000 {
        return 0.0;
    }
    let at = (frame % 48_000) as f32 / 48_000.0;
    (at * 440.0 * std::f32::consts::TAU).sin() * 0.25
}

pub fn start(data_dir: &Path) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let path = data_dir.join("memstats.csv");
    let spawned = thread::Builder::new()
        .name(String::from("memstats"))
        .spawn(move || write_every_minute(&path));
    if let Err(err) = spawned {
        eprintln!("booth: could not start the memstats thread: {err}");
    }
}

fn write_every_minute(path: &Path) {
    let started = Instant::now();
    let mut file = match OpenOptions::new().create(true).append(true).open(path) {
        Ok(file) => file,
        Err(err) => {
            eprintln!("booth: could not open {}: {err}", path.display());
            return;
        }
    };
    let _ = writeln!(file, "minute,kind,name,count,len,cap,largest");
    let mut minute = 0u64;
    loop {
        let mut out = String::new();
        for (name, value) in process() {
            out.push_str(&format!(
                "{minute},process,{name},1,{value},{value},{value}\n"
            ));
        }
        for total in stats::mem::totals() {
            out.push_str(&format!(
                "{minute},gauge,{},{},{},{},{}\n",
                total.name, total.instances, total.len, total.cap, total.largest
            ));
        }
        for (name, bytes, blocks) in heap() {
            out.push_str(&format!(
                "{minute},heap,{name},{blocks},{bytes},{bytes},{bytes}\n"
            ));
        }
        if let Err(err) = file.write_all(out.as_bytes()).and_then(|()| file.flush()) {
            eprintln!("booth: could not write {}: {err}", path.display());
        }
        minute += 1;
        let next = started + EVERY * minute as u32;
        thread::sleep(next.saturating_duration_since(Instant::now()));
    }
}

fn process() -> Vec<(&'static str, u64)> {
    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    let mut handles = 0u32;
    // SAFETY: the pseudo handle of this process needs no closing, and the
    // counters are as large as cb says.
    let (read, gdi, user) = unsafe {
        let me = GetCurrentProcess();
        let read = K32GetProcessMemoryInfo(
            me,
            (&raw mut counters).cast::<PROCESS_MEMORY_COUNTERS>(),
            counters.cb,
        ) != 0;
        GetProcessHandleCount(me, &mut handles);
        (
            read,
            GetGuiResources(me, GR_GDIOBJECTS),
            GetGuiResources(me, GR_USEROBJECTS),
        )
    };
    let mut out = Vec::new();
    if read {
        out.push(("working_set_kb", counters.WorkingSetSize as u64 / 1024));
        out.push(("private_kb", counters.PrivateUsage as u64 / 1024));
    }
    out.push(("handles", u64::from(handles)));
    out.push(("gdi_objects", u64::from(gdi)));
    out.push(("user_objects", u64::from(user)));
    let (bytes, blocks) = LIVE
        .iter()
        .zip(BLOCKS.iter())
        .map(|(bytes, blocks)| {
            (
                bytes.load(Ordering::Relaxed).max(0) as u64,
                blocks.load(Ordering::Relaxed).max(0) as u64,
            )
        })
        .fold((0, 0), |(b, n), (bytes, blocks)| (b + bytes, n + blocks));
    out.push(("heap_kb", bytes / 1024));
    out.push(("heap_blocks", blocks));
    out
}

pub fn egui(ctx: &Context) {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    static KEY: u8 = 0;
    {
        let mut last = LAST.lock().unwrap_or_else(PoisonError::into_inner);
        if last.is_some_and(|last| last.elapsed() < EGUI_EVERY) {
            return;
        }
        *last = Some(Instant::now());
    }
    let (textures, texels) = {
        let manager = ctx.tex_manager();
        let manager = manager.read();
        let texels: usize = manager
            .allocated()
            .map(|(_, meta)| meta.size[0] * meta.size[1])
            .sum();
        (manager.num_allocated(), texels)
    };
    let (atlas, galleys) = ctx.fonts(|fonts| {
        let [w, h] = fonts.font_image_size();
        (w * h, fonts.num_galleys_in_cache())
    });
    let data = ctx.memory(|memory| memory.data.len());
    stats::gauge!("egui.textures", &KEY, textures, texels);
    stats::gauge!("egui.font_atlas_texels", &KEY, atlas, atlas);
    stats::gauge!("egui.galleys", &KEY, galleys, galleys);
    stats::gauge!("egui.memory_data", &KEY, data, data);
}

#[global_allocator]
static COUNTING: Counting = Counting;

struct Counting;

const HEADER: usize = 16;
const NAMES: usize = 64;
// Kept for blocks taken before a thread's name was known, and for names
// past the table's end.
const UNNAMED: u8 = 0;
const OTHER: u8 = (NAMES - 1) as u8;
const NOT_LOOKED_UP: u8 = u8::MAX;
const NAME_UNITS: usize = 40;

static LIVE: [AtomicIsize; NAMES] = [const { AtomicIsize::new(0) }; NAMES];
static BLOCKS: [AtomicIsize; NAMES] = [const { AtomicIsize::new(0) }; NAMES];

struct Table {
    names: [[u16; NAME_UNITS]; NAMES],
    used: usize,
}

static TABLE: Mutex<Table> = Mutex::new(Table {
    names: [[0; NAME_UNITS]; NAMES],
    used: 1,
});

thread_local! {
    static TAG: Cell<u8> = const { Cell::new(NOT_LOOKED_UP) };
    static TRIES: Cell<u8> = const { Cell::new(0) };
}

// Where the block starts, and how far in the caller's part begins. The tag
// sits in the byte just before it.
fn widened(layout: Layout) -> Option<(Layout, usize)> {
    let offset = layout.align().max(HEADER);
    let outer = Layout::from_size_align(layout.size().checked_add(offset)?, offset).ok()?;
    Some((outer, offset))
}

fn count(tag: u8, bytes: isize, blocks: isize) {
    let at = usize::from(tag).min(NAMES - 1);
    LIVE[at].fetch_add(bytes, Ordering::Relaxed);
    BLOCKS[at].fetch_add(blocks, Ordering::Relaxed);
}

fn this_thread() -> u8 {
    TAG.try_with(|tag| {
        if tag.get() != NOT_LOOKED_UP {
            return tag.get();
        }
        let found = look_up();
        // A thread is named just after it starts, so a few blocks may come
        // before the name does. One that never gets a name stops asking.
        let tries = TRIES.try_with(|tries| {
            tries.set(tries.get().saturating_add(1));
            tries.get()
        });
        if found != UNNAMED || tries.unwrap_or(u8::MAX) >= 64 {
            tag.set(found);
        }
        found
    })
    .unwrap_or(UNNAMED)
}

// Nothing in here may take from the Rust heap: it runs inside the
// allocator. GetThreadDescription hands back memory of its own.
fn look_up() -> u8 {
    let mut name = [0u16; NAME_UNITS];
    let mut len = 0;
    // SAFETY: the description is a nul-terminated string Windows allocated,
    // read up to its nul and then given back with LocalFree.
    unsafe {
        let mut text = std::ptr::null_mut();
        if GetThreadDescription(GetCurrentThread(), &mut text) >= 0 && !text.is_null() {
            while len < NAME_UNITS && *text.add(len) != 0 {
                name[len] = *text.add(len);
                len += 1;
            }
            LocalFree(text as HLOCAL);
        }
    }
    // "dns query 3" and "dns query 4" count as one.
    while len > 0 && (name[len - 1] == u16::from(b' ') || (0x30..=0x39).contains(&name[len - 1])) {
        len -= 1;
    }
    if len == 0 {
        return UNNAMED;
    }
    let name = {
        let mut padded = [0u16; NAME_UNITS];
        padded[..len].copy_from_slice(&name[..len]);
        padded
    };
    let mut table = TABLE.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(at) = table.names[1..table.used]
        .iter()
        .position(|have| *have == name)
    {
        return (at + 1) as u8;
    }
    if table.used >= usize::from(OTHER) {
        return OTHER;
    }
    let at = table.used;
    table.names[at] = name;
    table.used += 1;
    at as u8
}

fn heap() -> Vec<(String, u64, u64)> {
    let names: Vec<String> = {
        let table = TABLE.lock().unwrap_or_else(PoisonError::into_inner);
        (0..NAMES)
            .map(|at| match at {
                0 => String::from("unnamed"),
                at if at == usize::from(OTHER) => String::from("other"),
                at if at < table.used => {
                    let name = &table.names[at];
                    let len = name
                        .iter()
                        .position(|unit| *unit == 0)
                        .unwrap_or(NAME_UNITS);
                    // A comma would split the line.
                    String::from_utf16_lossy(&name[..len]).replace(',', " ")
                }
                _ => String::new(),
            })
            .collect()
    };
    names
        .into_iter()
        .enumerate()
        .filter(|(at, name)| !name.is_empty() && BLOCKS[*at].load(Ordering::Relaxed) != 0)
        .map(|(at, name)| {
            (
                name,
                LIVE[at].load(Ordering::Relaxed).max(0) as u64,
                BLOCKS[at].load(Ordering::Relaxed).max(0) as u64,
            )
        })
        .collect()
}

// SAFETY: every block is System's, widened by a header that keeps the
// caller's alignment; dealloc and realloc find the same header again
// because Rust hands back the layout the block was made with.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let Some((outer, offset)) = widened(layout) else {
            return std::ptr::null_mut();
        };
        let base = unsafe { System.alloc(outer) };
        if base.is_null() {
            return base;
        }
        let tag = this_thread();
        let ptr = unsafe { base.add(offset) };
        unsafe { ptr.sub(1).write(tag) };
        count(tag, layout.size() as isize, 1);
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let Some((outer, offset)) = widened(layout) else {
            return std::ptr::null_mut();
        };
        let base = unsafe { System.alloc_zeroed(outer) };
        if base.is_null() {
            return base;
        }
        let tag = this_thread();
        let ptr = unsafe { base.add(offset) };
        unsafe { ptr.sub(1).write(tag) };
        count(tag, layout.size() as isize, 1);
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let (outer, offset) = widened(layout).expect("a block that was made has a layout");
        let tag = unsafe { ptr.sub(1).read() };
        count(tag, -(layout.size() as isize), -1);
        unsafe { System.dealloc(ptr.sub(offset), outer) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let (outer, offset) = widened(layout).expect("a block that was made has a layout");
        let Some(grown) = new_size.checked_add(offset) else {
            return std::ptr::null_mut();
        };
        let tag = unsafe { ptr.sub(1).read() };
        let base = unsafe { System.realloc(ptr.sub(offset), outer, grown) };
        if base.is_null() {
            return base;
        }
        count(tag, new_size as isize - layout.size() as isize, 0);
        unsafe { base.add(offset) }
    }
}
