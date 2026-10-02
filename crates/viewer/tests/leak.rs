// Opening and closing viewers leaves nothing behind. In its own test binary,
// so no other test's windows or devices are counted.

use std::time::{Duration, Instant};

use capture::Pattern;
use viewer::{ErrorKind, Frame, LinkState, Options, Show, Strip, Video, Viewer};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, TRUE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows::Win32::System::Threading::{
    GR_GDIOBJECTS, GR_USEROBJECTS, GetCurrentProcess, GetCurrentProcessId, GetGuiResources,
    GetProcessHandleCount,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowExW, GetClassNameW, GetWindowThreadProcessId, HWND_MESSAGE,
};
use windows::core::{BOOL, PCWSTR};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Counts {
    handles: u32,
    gdi: u32,
    user: u32,
    threads: u32,
}

fn counts() -> Counts {
    let mut handles = 0;
    // SAFETY: the pseudo handle for this process and live out parameters.
    // The handles are counted before the thread snapshot opens one.
    unsafe {
        let process = GetCurrentProcess();
        GetProcessHandleCount(process, &mut handles).unwrap();
        Counts {
            handles,
            gdi: GetGuiResources(process, GR_GDIOBJECTS),
            user: GetGuiResources(process, GR_USEROBJECTS),
            threads: threads(),
        }
    }
}

fn threads() -> u32 {
    // SAFETY: a snapshot of every thread, closed below, walked with an
    // entry whose size is set as Thread32First requires.
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else {
            return 0;
        };
        let ours = GetCurrentProcessId();
        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut count = 0;
        let mut more = Thread32First(snapshot, &mut entry).is_ok();
        while more {
            if entry.th32OwnerProcessID == ours {
                count += 1;
            }
            more = Thread32Next(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
        count
    }
}

// The windows this process owns, top level and message-only, as "class on
// thread N". Every viewer's window thread has ended by the time the test
// compares, so a window that appears in between belongs to something else
// in the process: Direct3D, the display driver or Windows itself.
fn own_windows() -> Vec<String> {
    extern "system" fn collect(hwnd: HWND, found: LPARAM) -> BOOL {
        // SAFETY: `found` is the Vec that own_windows passes to EnumWindows,
        // alive and not otherwise touched for the length of the call.
        let found = unsafe { &mut *(found.0 as *mut Vec<HWND>) };
        found.push(hwnd);
        TRUE
    }
    let mut all: Vec<HWND> = Vec::new();
    // SAFETY: the callback only pushes to `all`, which outlives the call.
    // FindWindowExW with HWND_MESSAGE walks the message-only windows.
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut all as *mut Vec<HWND> as isize));
        let mut after = None;
        while let Ok(hwnd) =
            FindWindowExW(Some(HWND_MESSAGE), after, PCWSTR::null(), PCWSTR::null())
        {
            all.push(hwnd);
            after = Some(hwnd);
        }
    }
    // SAFETY: a plain getter.
    let ours = unsafe { GetCurrentProcessId() };
    all.into_iter()
        .filter_map(|hwnd| {
            let mut process = 0;
            // SAFETY: a getter that answers 0 for a window gone since.
            let thread = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process)) };
            if process != ours {
                return None;
            }
            let mut class = [0u16; 256];
            // SAFETY: a getter writing at most the buffer's length.
            let length = unsafe { GetClassNameW(hwnd, &mut class) }.max(0) as usize;
            Some(format!(
                "{} on thread {thread}",
                String::from_utf16_lossy(&class[..length])
            ))
        })
        .collect()
}

// Open, show a few frames, close the window as a person would, drop.
fn one_viewer(index: usize) -> bool {
    let mut options = Options::new(format!("Booth viewer leak test {index}"), 320, 180);
    options.show = Show::NoActivate;
    let mut viewer = match Viewer::open(&options) {
        Ok(viewer) => viewer,
        Err(err)
            if err.kind() == ErrorKind::Other && err.to_string().contains("no graphics card") =>
        {
            println!("skipped: {err}");
            return false;
        }
        Err(err) => panic!("viewer {index}: {err}"),
    };
    let mut pattern = Pattern::new(viewer.device(), 320, 180, 0).unwrap();
    let strip = Strip {
        state: LinkState::Live,
        rtt_ms: Some(2.0),
        ..Strip::default()
    };
    for _ in 0..10 {
        let frame = pattern.next().unwrap();
        viewer
            .present(&Frame {
                video: Some(Video {
                    texture: &frame.texture,
                    index: 0,
                    width: frame.width,
                    height: frame.height,
                }),
                cursor: None,
                strip: &strip,
            })
            .unwrap();
    }
    drop(pattern);
    drop(viewer);
    true
}

const VIEWERS: usize = 20;

// Growth over all the viewers that is not the viewer's. While other
// programs are busy on the GPU, something in this process now and then
// keeps two more handles and, the first time, one more USER object, at any
// viewer, with no new thread and no window. On 2026-09-28 that was 5 runs
// of 92, never more than 4 handles and 1 USER object in a run: one of 10
// while another process opened and closed devices, none of 66 run alone.
// A leak in the viewer comes with every viewer, 20 or more; one in every
// other viewer is still 10.
const NOT_OURS: u32 = 6;

fn grew(before: Counts, after: Counts) -> (u32, u32, u32) {
    (
        after.handles.saturating_sub(before.handles),
        after.gdi.saturating_sub(before.gdi),
        after.user.saturating_sub(before.user),
    )
}

// GDI objects come from nothing here but the viewer, and never moved in
// any run, so those have to come out exact.
fn settled(before: Counts, after: Counts) -> bool {
    let (handles, gdi, user) = grew(before, after);
    handles <= NOT_OURS && gdi == 0 && user <= NOT_OURS
}

#[test]
fn viewers_opened_and_closed_in_a_row_leave_nothing_behind() {
    // The first viewer loads Direct3D, DXGI, Direct2D, DirectWrite and the
    // driver, registers the window class, and starts the threads they keep.
    if !one_viewer(0) {
        return;
    }
    // Some of the driver's own threads end a moment after the device goes.
    std::thread::sleep(Duration::from_millis(500));
    let before = counts();
    let windows_before = own_windows();
    let started = Instant::now();
    // After each viewer: a leak grows with every one, something the
    // process starts once is a single step.
    let mut steps = Vec::with_capacity(VIEWERS);
    for index in 1..=VIEWERS {
        one_viewer(index);
        steps.push(counts());
    }
    let took = started.elapsed();
    let mut after = counts();
    // Driver threads and their handles wind down on their own schedule.
    let waited = Instant::now();
    while grew(before, after) != (0, 0, 0) && waited.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(100));
        after = counts();
    }
    println!(
        "{VIEWERS} viewers opened and closed in {:.0} ms: before {before:?}, after {after:?}",
        took.as_secs_f64() * 1000.0
    );
    if grew(before, after) != (0, 0, 0) {
        println!("after each viewer: {steps:?}");
        println!("windows before: {windows_before:?}");
        println!("windows after: {:?}", own_windows());
    }
    let (handles, gdi, user) = grew(before, after);
    assert!(
        settled(before, after),
        "{VIEWERS} viewers left {handles} handles, {gdi} GDI objects and {user} USER objects behind"
    );
}
