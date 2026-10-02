// Alone in its file, so no other test's handles move the counts.

use std::time::{Duration, Instant};

use capture::{Capture, Monitor, Next, Options};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{
    DXGI_OUTDUPL_FRAME_INFO, IDXGIDevice, IDXGIOutput5, IDXGIOutputDuplication,
};
use windows::Win32::System::Threading::{
    GR_GDIOBJECTS, GR_USEROBJECTS, GetCurrentProcess, GetGuiResources, GetProcessHandleCount,
};
use windows::core::{IUnknown_Vtbl, Interface};

// Kernel handles, GDI objects, user objects.
fn counts() -> (u32, u32, u32) {
    let mut handles = 0;
    // SAFETY: the pseudo handle for this process needs no closing, and
    // `handles` is a live u32 for the call to fill.
    unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut handles) }
        .expect("could not read this process's handle count");
    // SAFETY: as above; the count is the return value.
    let gdi = unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };
    // SAFETY: as above.
    let user = unsafe { GetGuiResources(GetCurrentProcess(), GR_USEROBJECTS) };
    (handles, gdi, user)
}

// Every texture, view, shader, buffer and query made on a D3D11 device, and
// a duplication made with it, holds a reference to it. So when only our own
// reference is left, all of them were released.
fn references<T: Interface>(object: &T) -> u32 {
    let raw = object.as_raw();
    // SAFETY: `raw` is a live COM object, whose first field is its vtable,
    // which starts with IUnknown's. The AddRef is undone by the Release,
    // which returns the count.
    unsafe {
        let vtable = *(raw as *const *const IUnknown_Vtbl);
        ((*vtable).AddRef)(raw);
        ((*vtable).Release)(raw)
    }
}

const RUN: Duration = Duration::from_millis(50);

fn run_a_little(capture: &mut Capture) {
    let start = Instant::now();
    while start.elapsed() < RUN {
        if let Next::Paused(reason) = capture.next().unwrap() {
            println!("paused: {reason}");
        }
    }
}

// What Windows itself keeps: the device Capture makes and a duplication of
// the monitor, straight through the windows crate, frames taken and handed
// back unread for as long as run_a_little runs.
fn open_without_capture(monitor: &Monitor) {
    let device = capture::device_on(&monitor.adapter).unwrap();
    let dxgi: IDXGIDevice = device.cast().unwrap();
    // SAFETY: getters on live interfaces, and EnumOutputs takes a plain
    // index and fails past the end.
    let output = unsafe {
        let adapter = dxgi.GetAdapter().unwrap();
        (0..)
            .map_while(|index| adapter.EnumOutputs(index).ok())
            .find(|output| {
                let name = output.GetDesc().unwrap().DeviceName;
                let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
                String::from_utf16_lossy(&name[..end]) == monitor.id.device_name
            })
            .expect("the monitor's output is gone")
    };
    let output: IDXGIOutput5 = output.cast().unwrap();
    // SAFETY: a live device on the adapter this output belongs to, and a
    // format list that outlives the call.
    let duplication: IDXGIOutputDuplication =
        unsafe { output.DuplicateOutput1(&device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM]) }.unwrap();
    let start = Instant::now();
    while start.elapsed() < RUN {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        // SAFETY: both out parameters are live locals, and a frame acquired
        // is released straight away without being looked at.
        unsafe {
            if duplication
                .AcquireNextFrame(10, &mut info, &mut resource)
                .is_ok()
            {
                drop(resource);
                duplication.ReleaseFrame().unwrap();
            }
        }
    }
}

// Handle counts wobble by a handle or two as the D3D runtime and the driver
// start and end worker threads.
const WOBBLE: u32 = 2;

#[test]
fn opening_the_same_monitor_again_and_again_leaks_nothing() {
    capture::make_process_dpi_aware().unwrap();
    if capture::adapters().unwrap().is_empty() {
        println!("skipped: this PC has no hardware graphics adapter");
        return;
    }
    let monitors = capture::monitors().unwrap();
    let Some(monitor) = monitors.iter().find(|m| m.primary).or(monitors.first()) else {
        println!("skipped: no monitor is attached to the desktop");
        return;
    };
    // Whatever Windows and the driver load once per process is loaded here.
    for _ in 0..2 {
        let mut capture = Capture::open(monitor, Options::default()).unwrap();
        run_a_little(&mut capture);
        drop(capture);
        open_without_capture(monitor);
    }

    let windows_before = counts();
    for _ in 0..5 {
        open_without_capture(monitor);
    }
    let windows_after = counts();
    let windows_keeps = windows_after.0.saturating_sub(windows_before.0);

    let before = counts();
    let mut rounds = Vec::new();
    for round in 0..5 {
        let mut capture = Capture::open(monitor, Options::default()).unwrap();
        run_a_little(&mut capture);
        let device = capture.device().clone();
        let while_open = references(&device);
        drop(capture);
        assert_eq!(
            references(&device),
            1,
            "round {round}: something made on the capture device outlived the capture ({while_open} references while open)"
        );
        drop(device);
        rounds.push(counts());
    }
    let after = counts();
    println!(
        "handles, gdi objects, user objects: 5 opens without Capture {windows_before:?} to {windows_after:?}; with Capture {before:?}, after each round {rounds:?}"
    );
    assert!(
        after.0 <= before.0 + windows_keeps + WOBBLE,
        "handles went from {} to {} over 5 opens, where Windows alone kept {windows_keeps}",
        before.0,
        after.0
    );
    assert!(
        after.1 <= before.1,
        "gdi objects went from {} to {} over 5 opens",
        before.1,
        after.1
    );
    assert!(
        after.2 <= before.2,
        "user objects went from {} to {} over 5 opens",
        before.2,
        after.2
    );
}
