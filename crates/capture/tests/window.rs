// A window share through Windows.Graphics.Capture, on a plain window this
// test makes and moves about. Numbers only, as for the desktop: sizes,
// pauses and timings, never the pixels.

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use capture::{Next, Options, PauseReason, WindowCapture};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DispatchMessageW, IsWindow, MSG, PM_REMOVE, PeekMessageW, PostMessageW,
    SW_MINIMIZE, SW_RESTORE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SetWindowPos,
    ShowWindowAsync, TranslateMessage, WINDOW_STYLE, WM_CLOSE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_OVERLAPPEDWINDOW, WS_POPUP, WS_VISIBLE,
};
use windows::core::w;

// A window on its own thread, which pumps its messages until it is closed.
struct Window(isize);

impl Window {
    // Borderless, so the picture is exactly this size.
    fn open(width: i32, height: i32) -> Window {
        Window::with_style(WS_POPUP, width, height)
    }

    fn with_style(style: WINDOW_STYLE, width: i32, height: i32) -> Window {
        let (sent, got) = mpsc::channel();
        thread::spawn(move || {
            // SAFETY: a system class, constant strings, no parent.
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                    w!("STATIC"),
                    w!("Booth window capture test"),
                    style | WS_VISIBLE,
                    120,
                    120,
                    width,
                    height,
                    None,
                    None,
                    None,
                    None,
                )
            }
            .unwrap();
            sent.send(hwnd.0 as isize).unwrap();
            let mut msg = MSG::default();
            // SAFETY: a window of this thread; a gone one answers false.
            while unsafe { IsWindow(Some(hwnd)) }.as_bool() {
                // SAFETY: a live MSG for each call.
                while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                    unsafe {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                thread::sleep(Duration::from_millis(5));
            }
        });
        Window(got.recv().unwrap())
    }

    fn hwnd(&self) -> HWND {
        HWND(self.0 as *mut _)
    }

    fn resize(&self, width: i32, height: i32) {
        // SAFETY: a window of this process; the move is sent to its thread.
        unsafe {
            SetWindowPos(
                self.hwnd(),
                None,
                0,
                0,
                width,
                height,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            )
        }
        .unwrap();
    }

    fn show(&self, how: windows::Win32::UI::WindowsAndMessaging::SHOW_WINDOW_CMD) {
        // SAFETY: a window of this process; posted to its thread.
        let _ = unsafe { ShowWindowAsync(self.hwnd(), how) };
    }

    fn close(&self) {
        // SAFETY: posted; the window's own thread destroys it.
        let _ = unsafe { PostMessageW(Some(self.hwnd()), WM_CLOSE, WPARAM(0), LPARAM(0)) };
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        self.close();
    }
}

fn adapter() -> Option<capture::Adapter> {
    capture::make_process_dpi_aware().unwrap();
    let adapters = capture::adapters().unwrap();
    if adapters.is_empty() {
        println!("skipped: this PC has no hardware graphics adapter");
        return None;
    }
    if capture::monitors().unwrap().is_empty() {
        println!("skipped: no monitor is attached to the desktop");
        return None;
    }
    adapters.into_iter().next()
}

// The next Frame's size, or what came instead, within `within`.
fn frame_size(capture: &mut WindowCapture, within: Duration) -> Result<(u32, u32), String> {
    let start = Instant::now();
    while start.elapsed() < within {
        match capture.next() {
            Ok(Next::Frame(frame)) => return Ok((frame.width, frame.height)),
            Ok(Next::Idle | Next::Cursor(_)) => {}
            Ok(Next::Paused(why)) => return Err(format!("paused: {why}")),
            Err(err) => return Err(err.to_string()),
        }
    }
    Err(format!("no frame in {} ms", within.as_millis()))
}

// Every size that comes within `within`, the last one last.
fn sizes_for(capture: &mut WindowCapture, within: Duration) -> Vec<(u32, u32)> {
    let mut sizes = Vec::new();
    let start = Instant::now();
    while start.elapsed() < within {
        if let Ok(Next::Frame(frame)) = capture.next() {
            sizes.push((frame.width, frame.height));
        }
    }
    sizes
}

#[test]
fn a_window_is_captured_at_its_own_size() {
    let Some(adapter) = adapter() else { return };
    let window = Window::open(640, 360);
    let opened = Instant::now();
    let mut capture = WindowCapture::open(window.0, &adapter, Options::default()).unwrap();
    let first = frame_size(&mut capture, Duration::from_secs(2));
    println!(
        "first frame {first:?} {:.1} ms after opening",
        opened.elapsed().as_secs_f64() * 1000.0
    );
    assert_eq!(first, Ok((640, 360)));
    assert_eq!((capture.plan().width, capture.plan().height), (640, 360));
}

#[test]
fn a_window_scaled_down_into_the_box() {
    let Some(adapter) = adapter() else { return };
    let window = Window::open(1200, 600);
    let options = Options {
        max_width: 800,
        max_height: 800,
        max_fps: 0,
    };
    let mut capture = WindowCapture::open(window.0, &adapter, options).unwrap();
    assert_eq!(
        frame_size(&mut capture, Duration::from_secs(2)),
        Ok((800, 400))
    );
}

#[test]
fn a_resized_window_comes_out_at_its_new_size() {
    let Some(adapter) = adapter() else { return };
    let window = Window::open(640, 360);
    let mut capture = WindowCapture::open(window.0, &adapter, Options::default()).unwrap();
    assert_eq!(
        frame_size(&mut capture, Duration::from_secs(2)),
        Ok((640, 360))
    );
    window.resize(800, 480);
    let sizes = sizes_for(&mut capture, Duration::from_secs(1));
    println!("after one resize: {sizes:?}");
    assert_eq!(sizes.last(), Some(&(800, 480)), "{sizes:?}");
    // Two quick sizes, then nothing: the last one still has to come out.
    window.resize(900, 500);
    thread::sleep(Duration::from_millis(30));
    window.resize(1000, 560);
    let sizes = sizes_for(&mut capture, Duration::from_secs(1));
    println!("after two quick resizes: {sizes:?}");
    assert_eq!(sizes.last(), Some(&(1000, 560)), "{sizes:?}");
}

#[test]
fn a_minimized_window_pauses_and_comes_back() {
    let Some(adapter) = adapter() else { return };
    // With a title bar and borders, as most windows shared are.
    let window = Window::with_style(WS_OVERLAPPEDWINDOW, 640, 360);
    let mut capture = WindowCapture::open(window.0, &adapter, Options::default()).unwrap();
    let size = frame_size(&mut capture, Duration::from_secs(2)).unwrap();
    println!("a 640x360 window with a title bar comes out {size:?}");
    window.show(SW_MINIMIZE);
    let start = Instant::now();
    let mut paused = None;
    while start.elapsed() < Duration::from_secs(2) && paused.is_none() {
        match capture.next().unwrap() {
            Next::Paused(why) => paused = Some(why),
            Next::Frame(frame) => println!("while minimizing: {}x{}", frame.width, frame.height),
            _ => {}
        }
    }
    assert_eq!(paused, Some(PauseReason::Minimized));
    // Told once: the rest of the pause is idle.
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(500) {
        let next = capture.next().unwrap();
        assert!(matches!(next, Next::Idle), "{next:?}");
    }
    window.show(SW_RESTORE);
    let sizes = sizes_for(&mut capture, Duration::from_secs(1));
    println!("after restoring: {sizes:?}");
    assert!(!sizes.is_empty());
    assert!(sizes.iter().all(|&each| each == size), "{sizes:?}");
}

#[test]
fn a_closed_window_ends_the_capture() {
    let Some(adapter) = adapter() else { return };
    let window = Window::open(640, 360);
    let mut capture = WindowCapture::open(window.0, &adapter, Options::default()).unwrap();
    assert_eq!(
        frame_size(&mut capture, Duration::from_secs(2)),
        Ok((640, 360))
    );
    window.close();
    let start = Instant::now();
    let ended = loop {
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "still capturing 2 s after the window closed"
        );
        if let Err(err) = capture.next() {
            break err.to_string();
        }
    };
    assert_eq!(ended, "the shared window was closed");
}
