// A window share through Windows.Graphics.Capture, on a plain window this
// test makes and moves about. Numbers only, as for the desktop: sizes,
// pauses and timings, never the pixels.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use capture::{Next, Options, PauseReason, WINDOW_GONE, WindowCapture, WindowId};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DispatchMessageW, IsWindow, MSG, PM_REMOVE, PeekMessageW, PostMessageW,
    SW_MINIMIZE, SW_RESTORE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER,
    SetWindowPos, SetWindowTextW, ShowWindowAsync, TranslateMessage, WINDOW_STYLE, WM_CLOSE,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_OVERLAPPEDWINDOW, WS_POPUP, WS_VISIBLE,
};
use windows::core::{HSTRING, w};

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

    fn id(&self) -> WindowId {
        WindowId::of(self.0).unwrap()
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

// The tests that minimize a window, or time its pictures, run alone, the
// rest side by side. A window minimized or restored beside the fast window
// holds up its pictures, about 10 more of 240 held, and windows changing
// beside a restore held up in the drain change which of its pictures
// Windows keeps: 4 tries in 9 found none of the restored window's own,
// against 1 in 11 alone.
static ALONE: RwLock<()> = RwLock::new(());

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
    let _turn = ALONE.read().unwrap_or_else(|p| p.into_inner());
    let Some(adapter) = adapter() else { return };
    let window = Window::open(640, 360);
    let opened = Instant::now();
    let mut capture = WindowCapture::open(window.id(), &adapter, Options::default()).unwrap();
    let first = frame_size(&mut capture, Duration::from_secs(2));
    println!(
        "first frame {first:?} {:.1} ms after opening",
        opened.elapsed().as_secs_f64() * 1000.0
    );
    assert_eq!(first, Ok((640, 360)));
    assert_eq!((capture.plan().width, capture.plan().height), (640, 360));
    // What Windows answers an unpackaged program about its yellow border,
    // recorded on every run.
    println!("{}", capture.border());
}

// With a title bar and borders, the pictures are DWM's frame without the
// shadow, smaller than the window's own rectangle: the share opens at that
// size, so its first frame needs no second encoder.
#[test]
fn a_bordered_window_opens_at_the_size_of_its_pictures() {
    let _turn = ALONE.read().unwrap_or_else(|p| p.into_inner());
    let Some(adapter) = adapter() else { return };
    let window = Window::with_style(WS_OVERLAPPEDWINDOW, 640, 360);
    let capture = WindowCapture::open(window.id(), &adapter, Options::default()).unwrap();
    let opened = (capture.plan().width, capture.plan().height);
    let mut capture = capture;
    let first = frame_size(&mut capture, Duration::from_secs(2));
    println!("a 640x360 window with a title bar opens at {opened:?}, first frame {first:?}");
    assert_eq!(first, Ok(opened));
}

#[test]
fn a_window_names_the_monitor_it_is_on() {
    let _turn = ALONE.read().unwrap_or_else(|p| p.into_inner());
    let Some(_) = adapter() else { return };
    let window = Window::open(400, 300);
    let monitor = capture::window_monitor(window.id()).expect("no monitor for the window");
    // The helper puts its windows at 120,120 on the desktop.
    let (left, top) = (monitor.left, monitor.top);
    assert!(
        (left..left + monitor.width as i32).contains(&120)
            && (top..top + monitor.height as i32).contains(&120),
        "{monitor:?}"
    );
}

// A window closed before the share opens is refused with the sentence the
// room shows, and a handle that names no window is no window.
#[test]
fn a_closed_window_does_not_open() {
    let _turn = ALONE.read().unwrap_or_else(|p| p.into_inner());
    let Some(adapter) = adapter() else { return };
    assert_eq!(WindowId::of(0), None);
    let window = Window::open(320, 240);
    let id = window.id();
    window.close();
    let start = Instant::now();
    while id.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "the window did not close"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let Err(err) = WindowCapture::open(id, &adapter, Options::default()) else {
        panic!("a closed window opened");
    };
    assert_eq!(err.to_string(), WINDOW_GONE);
}

#[test]
fn a_window_scaled_down_into_the_box() {
    let _turn = ALONE.read().unwrap_or_else(|p| p.into_inner());
    let Some(adapter) = adapter() else { return };
    let window = Window::open(1200, 600);
    let options = Options {
        max_width: 800,
        max_height: 800,
        max_fps: 0,
    };
    let mut capture = WindowCapture::open(window.id(), &adapter, options).unwrap();
    assert_eq!(
        frame_size(&mut capture, Duration::from_secs(2)),
        Ok((800, 400))
    );
}

// Under 256 a side the frame is 256 with the window at its top left, and it
// keeps that size while the window changes inside it.
#[test]
fn a_small_window_is_padded_to_256() {
    let _turn = ALONE.read().unwrap_or_else(|p| p.into_inner());
    let Some(adapter) = adapter() else { return };
    let window = Window::open(200, 120);
    let mut capture = WindowCapture::open(window.id(), &adapter, Options::default()).unwrap();
    assert_eq!(
        frame_size(&mut capture, Duration::from_secs(2)),
        Ok((256, 256))
    );
    assert_eq!(capture.plan().picture, (200, 120));
    window.resize(300, 200);
    let sizes = sizes_for(&mut capture, Duration::from_secs(1));
    assert_eq!(sizes.last(), Some(&(300, 256)), "{sizes:?}");
    assert_eq!(capture.plan().picture, (300, 200));
    window.resize(640, 360);
    let sizes = sizes_for(&mut capture, Duration::from_secs(1));
    assert_eq!(sizes.last(), Some(&(640, 360)), "{sizes:?}");
    assert_eq!(capture.plan().picture, (640, 360));
}

#[test]
fn a_resized_window_comes_out_at_its_new_size() {
    let _turn = ALONE.read().unwrap_or_else(|p| p.into_inner());
    let Some(adapter) = adapter() else { return };
    let window = Window::open(640, 360);
    let mut capture = WindowCapture::open(window.id(), &adapter, Options::default()).unwrap();
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

// Kept in a frame, the window comes out at the frame's size whatever its
// own, while natural() says its own; once it stands still, again() makes
// its newest picture at its own size.
#[test]
fn a_window_kept_in_a_frame() {
    let _turn = ALONE.read().unwrap_or_else(|p| p.into_inner());
    let Some(adapter) = adapter() else { return };
    let window = Window::open(640, 360);
    let mut capture = WindowCapture::open(window.id(), &adapter, Options::default()).unwrap();
    assert_eq!(
        frame_size(&mut capture, Duration::from_secs(2)),
        Ok((640, 360))
    );
    assert_eq!(capture.again().unwrap().map(|frame| frame.number), None);
    capture.set_frame(Some((640, 360)));
    window.resize(800, 480);
    let sizes = sizes_for(&mut capture, Duration::from_secs(1));
    assert!(
        !sizes.is_empty() && sizes.iter().all(|&size| size == (640, 360)),
        "{sizes:?}"
    );
    assert_eq!(capture.natural(), Some((800, 480)));
    assert_eq!(capture.plan().picture, (600, 360));
    window.resize(400, 200);
    let sizes = sizes_for(&mut capture, Duration::from_secs(1));
    assert_eq!(sizes.last(), Some(&(640, 360)), "{sizes:?}");
    assert_eq!(capture.plan().picture, (400, 200));
    window.resize(1000, 600);
    sizes_for(&mut capture, Duration::from_secs(1));
    capture.set_frame(None);
    let frame = capture.again().unwrap().expect("no picture kept");
    assert_eq!((frame.width, frame.height), (1000, 600));
    assert!(frame.present.elapsed() < Duration::from_millis(100));
    // Kept for another try until a picture goes out at its own size.
    let frame = capture.again().unwrap().expect("no picture kept");
    assert_eq!((frame.width, frame.height), (1000, 600));
    window.resize(900, 500);
    let sizes = sizes_for(&mut capture, Duration::from_secs(1));
    assert_eq!(sizes.last(), Some(&(900, 500)), "{sizes:?}");
    assert!(capture.again().unwrap().is_none());
}

// A window whose text changes as fast as its thread paints, about 180 times
// a second, shared at 120: more than 60 frames a second where Windows takes
// a shorter interval than its own pace, build 26100 and later, and no more
// than 60 where it does not. The frames the fps cap held and sent later are
// counted: judged by the moments frames arrive, a quarter of them were;
// judged by Windows' stamps, from 1 in 40 to 1 in 10, the holds a change
// rate off the refresh grid makes anyway.
#[test]
fn a_fast_window_comes_at_the_rate_asked() {
    let _turn = ALONE.write().unwrap_or_else(|p| p.into_inner());
    let Some(adapter) = adapter() else { return };
    let window = Window::open(640, 360);
    let options = Options {
        max_width: 0,
        max_height: 0,
        max_fps: 120,
    };
    let mut capture = WindowCapture::open(window.id(), &adapter, options).unwrap();
    frame_size(&mut capture, Duration::from_secs(2)).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let changes = Arc::new(AtomicU64::new(0));
    let changer = {
        let (stop, changes, hwnd) = (Arc::clone(&stop), Arc::clone(&changes), window.0);
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let text = HSTRING::from(format!("{}", changes.load(Ordering::Relaxed)));
                // SAFETY: sent to the window's own thread, which paints it.
                let _ = unsafe { SetWindowTextW(HWND(hwnd as *mut _), &text) };
                changes.fetch_add(1, Ordering::Relaxed);
            }
        })
    };
    thread::sleep(Duration::from_millis(200));
    let (mut frames, mut held) = (0u32, 0u32);
    let changed = changes.load(Ordering::Relaxed);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if let Next::Frame(frame) = capture.next().unwrap() {
            frames += 1;
            if frame.converted.elapsed() > Duration::from_millis(1) {
                held += 1;
            }
        }
    }
    let changed = changes.load(Ordering::Relaxed) - changed;
    stop.store(true, Ordering::Relaxed);
    changer.join().unwrap();
    let seconds = start.elapsed().as_secs_f64();
    let fps = f64::from(frames) / seconds;
    println!(
        "{:.0} changes a second, {fps:.1} frames a second, {held} of {frames} sent after being held; {}",
        changed as f64 / seconds,
        capture
            .pacing_note()
            .unwrap_or("Windows takes a shorter interval")
    );
    assert!(
        changed as f64 / seconds > 150.0,
        "the window changed too slowly"
    );
    // DWM composes no faster than the monitor's refresh.
    let refresh = capture::window_monitor(window.id()).map_or(60.0, |monitor| monitor.refresh_hz);
    if capture.pacing_note().is_some() {
        assert!(fps < 62.0, "{fps:.1} frames a second");
    } else {
        assert!(fps > refresh.min(120.0) * 0.9, "{fps:.1} frames a second");
        assert!(held * 5 < frames, "{held} of {frames} frames were held");
    }
}

#[test]
fn a_minimized_window_pauses_and_comes_back() {
    let _turn = ALONE.write().unwrap_or_else(|p| p.into_inner());
    let Some(adapter) = adapter() else { return };
    // With a title bar and borders, as most windows shared are.
    let window = Window::with_style(WS_OVERLAPPEDWINDOW, 640, 360);
    let mut capture = WindowCapture::open(window.id(), &adapter, Options::default()).unwrap();
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
    // Told once: the rest of the pause is idle, but for the pointer hidden
    // from the watchers' frozen picture.
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(500) {
        let next = capture.next().unwrap();
        assert!(
            matches!(&next, Next::Idle) || matches!(&next, Next::Cursor(update) if !update.visible),
            "{next:?}"
        );
    }
    window.show(SW_RESTORE);
    let sizes = sizes_for(&mut capture, Duration::from_secs(1));
    println!("after restoring: {sizes:?}");
    assert!(!sizes.is_empty());
    assert!(sizes.iter().all(|&each| each == size), "{sizes:?}");
}

// A restore that lands while a pass that found the window minimized is held
// up, here for 1 s, still comes out, with the restore's own pictures and not
// only the one Windows sends once the drain makes room in the pool. When the
// restore's first two pictures are its caption and a size in between, that
// late one is the first there is of the restored window, so up to three
// restores are tried for one that shows the rest.
#[cfg(feature = "fault")]
#[test]
fn restore_during_the_drain() {
    let _turn = ALONE.write().unwrap_or_else(|p| p.into_inner());
    let Some(adapter) = adapter() else { return };
    let window = Window::with_style(WS_OVERLAPPEDWINDOW, 640, 360);
    let mut capture = WindowCapture::open(window.id(), &adapter, Options::default()).unwrap();
    let size = frame_size(&mut capture, Duration::from_secs(2)).unwrap();
    let hold = Duration::from_secs(1);
    let mut early = false;
    for _ in 0..3 {
        window.show(SW_MINIMIZE);
        let start = Instant::now();
        let mut paused = None;
        while start.elapsed() < Duration::from_secs(2) && paused.is_none() {
            if let Next::Paused(why) = capture.next().unwrap() {
                paused = Some(why);
            }
        }
        assert_eq!(paused, Some(PauseReason::Minimized));
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(300) {
            capture.next().unwrap();
        }
        capture::fault::set_hold_after_minimized(window.0, hold);
        let armed = Instant::now();
        // Restored without being made the active window: with SW_RESTORE, a
        // test process that had opened other windows got none of the
        // restored window's own pictures in every try of 4 runs in 10, and
        // without, never in two tries running in 14.
        let frames = thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Duration::from_millis(100));
                window.show(SW_SHOWNOACTIVATE);
            });
            let mut frames = Vec::new();
            while armed.elapsed() < Duration::from_secs(3) {
                if let Next::Frame(frame) = capture.next().unwrap() {
                    let at = frame.present.saturating_duration_since(armed);
                    frames.push(((frame.width, frame.height), at, armed.elapsed()));
                }
            }
            frames
        });
        println!("the window comes out {size:?}; after the restore {frames:?}");
        let Some(&(_, first, came)) = frames.first() else {
            panic!("no frame in the 3 s after the restore");
        };
        assert!(frames.iter().all(|&(each, ..)| each == size), "{frames:?}");
        // The first picture is from well before the hold ends: one of the
        // restore's own, not one Windows sent once the pass was through.
        // Coming back only after the hold says the restore did land in it.
        if first < Duration::from_millis(900) && came >= hold {
            early = true;
            break;
        }
    }
    assert!(
        early,
        "no restore came out with a picture from before the hold ended"
    );
}

// A window minimized and restored again within one pass, here held up for
// 1 s, is no pause, and what the restore sends at other sizes stays out.
#[cfg(feature = "fault")]
#[test]
fn minimized_and_restored_within_one_pass() {
    let _turn = ALONE.write().unwrap_or_else(|p| p.into_inner());
    let Some(adapter) = adapter() else { return };
    let window = Window::with_style(WS_OVERLAPPEDWINDOW, 640, 360);
    let mut capture = WindowCapture::open(window.id(), &adapter, Options::default()).unwrap();
    let size = frame_size(&mut capture, Duration::from_secs(2)).unwrap();
    let hold = Duration::from_secs(1);
    capture::fault::set_hold_after_minimized(window.0, hold);
    let minimized = Instant::now();
    let frames = thread::scope(|scope| {
        scope.spawn(|| {
            window.show(SW_MINIMIZE);
            thread::sleep(Duration::from_millis(300));
            window.show(SW_RESTORE);
        });
        let mut frames = Vec::new();
        while minimized.elapsed() < Duration::from_secs(3) {
            match capture.next().unwrap() {
                Next::Frame(frame) => {
                    frames.push(((frame.width, frame.height), minimized.elapsed()));
                }
                Next::Paused(why) => panic!("paused for {why:?}: {frames:?}"),
                _ => {}
            }
        }
        frames
    });
    println!("the window comes out {size:?}; minimized and restored {frames:?}");
    // The pass that saw the window minimized held everything up from well
    // before the restore: what comes back after the hold came out of it.
    let after: Vec<_> = frames.iter().filter(|&&(_, came)| came >= hold).collect();
    assert!(!after.is_empty(), "no frame after the hold: {frames:?}");
    assert!(after.iter().all(|&&(each, _)| each == size), "{frames:?}");
}

#[test]
fn a_closed_window_ends_the_capture() {
    let _turn = ALONE.read().unwrap_or_else(|p| p.into_inner());
    let Some(adapter) = adapter() else { return };
    let window = Window::open(640, 360);
    let mut capture = WindowCapture::open(window.id(), &adapter, Options::default()).unwrap();
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
