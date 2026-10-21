// The viewer's own window, shown without taking the focus, fed the capture
// crate's test pattern. Nothing here reads the screen.

use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

use capture::Pattern;
use stats::{Level, TraceSample};
use viewer::{
    Cursor, CursorKind, CursorShape, ErrorKind, Frame, LinkState, Options, PathWord, PresentPath,
    Show, Strip, Video, Viewer,
};
use windows::Win32::Foundation::{LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetMenuState, GetSystemMenu, GetWindowLongPtrW, GetWindowRect, MF_BYCOMMAND,
    MF_CHECKED, PostMessageW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SendMessageW, SetWindowPos,
    WM_CLOSE, WM_NULL, WM_SYSCOMMAND, WS_EX_TOPMOST,
};

// The viewer's system menu item for small on top.
const SMALL_ON_TOP: usize = 0x0010;
const FRAMES: usize = 240;
const RESIZE_AT: usize = 120;

// One viewer at a time, as Booth has it. With these tests side by side, two
// never shown windows went fullscreen on the same monitor while the shown
// one presented, on the same device, and in about one run in four every
// other present of the shown one then blocked for two seconds with S_OK,
// for the rest of the run (2026-09-28). Neither hidden test next to the
// shown one did it alone, in 15 runs each.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn one_at_a_time() -> MutexGuard<'static, ()> {
    ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn open(show: Show, width: u32, height: u32) -> Option<Viewer> {
    // The viewer's window is per-monitor aware; this thread has to be too,
    // or Windows scales every rectangle it reports here.
    // SAFETY: a constant context; the previous one is not needed.
    unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let mut options = Options::new("Booth viewer test", width, height);
    options.show = show;
    match Viewer::open(&options) {
        Ok(viewer) => Some(viewer),
        Err(err)
            if err.kind() == ErrorKind::Other && err.to_string().contains("no graphics card") =>
        {
            println!("skipped: {err}");
            None
        }
        Err(err) => panic!("{err}"),
    }
}

fn strip() -> Strip {
    Strip {
        state: LinkState::Live,
        rtt_ms: Some(3.0),
        jitter_ms: Some(0.3),
        loss_pct: Some(0.0),
        path: Some(PathWord::Lan),
        trace: (0..120).map(|_| TraceSample::Rtt(3.0)).collect(),
        encode_ms: Some(2.4),
        decode_ms: Some(1.2),
        end_to_end_ms: Some(9.0),
        end_to_end_level: Level::Good,
        ..Strip::default()
    }
}

fn pointer() -> Cursor {
    let mut bytes = Vec::new();
    for y in 0..24u32 {
        for x in 0..24u32 {
            let alpha = if x <= y { 255 } else { 0 };
            bytes.extend_from_slice(&[255, 255, 255, alpha]);
        }
    }
    Cursor {
        x: 40,
        y: 40,
        visible: true,
        scale: 1.0,
        shape: Some(CursorShape {
            kind: CursorKind::Color,
            width: 24,
            height: 24,
            pitch: 96,
            hotspot_x: 0,
            hotspot_y: 0,
            bytes,
        }),
    }
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn percentile(sorted: &[Duration], share: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * share).round() as usize]
}

fn window_rect(viewer: &Viewer) -> RECT {
    let mut rect = RECT::default();
    // SAFETY: the viewer's live window and a live out parameter.
    unsafe { GetWindowRect(viewer.window(), &mut rect) }.unwrap();
    rect
}

fn monitor_info(viewer: &Viewer) -> MONITORINFO {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: plain lookups on the viewer's live window.
    unsafe {
        let monitor = MonitorFromWindow(viewer.window(), MONITOR_DEFAULTTONEAREST);
        assert!(GetMonitorInfoW(monitor, &mut info).as_bool());
    }
    info
}

fn monitor_rect(viewer: &Viewer) -> RECT {
    monitor_info(viewer).rcMonitor
}

fn work_rect(viewer: &Viewer) -> RECT {
    monitor_info(viewer).rcWork
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "gave up waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn presents_through_a_resize_and_a_close() {
    let _screen = one_at_a_time();
    let Some(mut viewer) = open(Show::NoActivate, 640, 360) else {
        return;
    };
    println!(
        "on {}, tearing {}",
        viewer.adapter(),
        if viewer.tearing() {
            "allowed"
        } else {
            "not allowed"
        }
    );
    let mut pattern = Pattern::new(viewer.device(), 640, 360, 120).unwrap();
    let strip = strip();
    let pointer = pointer();
    let mut took = Vec::with_capacity(FRAMES);
    let mut paths: Vec<(usize, PresentPath)> = Vec::new();
    let mut known_again_after = None;
    let before = viewer.buffer_size();
    for index in 0..FRAMES {
        if index == RESIZE_AT {
            // SAFETY: the viewer's live window. The resize happens on the
            // window's thread, which answers while this one waits.
            unsafe {
                SetWindowPos(
                    viewer.window(),
                    None,
                    0,
                    0,
                    820,
                    560,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                )
            }
            .unwrap();
            assert!(viewer.needs_redraw());
        }
        let frame = pattern.next().unwrap();
        let video = Video {
            texture: &frame.texture,
            index: 0,
            width: frame.width,
            height: frame.height,
        };
        let presented = viewer
            .present(&Frame {
                video: Some(video),
                cursor: (index == 0).then_some(&pointer),
                strip: &strip,
            })
            .unwrap_or_else(|err| panic!("frame {index}: {err}"));
        assert!(presented.shown);
        took.push(presented.took);
        if let Some(path) = presented.path {
            if paths.last().map(|(_, last)| *last) != Some(path) {
                paths.push((index, path));
            }
            if index >= RESIZE_AT && known_again_after.is_none() {
                known_again_after = Some(index - RESIZE_AT);
            }
        }
    }
    let after = viewer.buffer_size();
    assert_ne!(
        before, after,
        "the buffers kept their size through a resize"
    );
    let mut client = RECT::default();
    // SAFETY: the viewer's live window and a live out parameter.
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetClientRect(viewer.window(), &mut client) }
        .unwrap();
    assert_eq!(
        after,
        (client.right as u32, client.bottom as u32),
        "the buffers are not the client area's size"
    );

    took.sort();
    println!(
        "{FRAMES} frames: present took median {:.3} ms, 95th {:.3} ms, longest {:.3} ms",
        ms(percentile(&took, 0.5)),
        ms(percentile(&took, 0.95)),
        ms(percentile(&took, 1.0))
    );
    println!("buffers {before:?} then {after:?}");
    println!("present path seen: {paths:?} (frame index, path)");
    println!(
        "after the resize the path was known again {} frames later",
        known_again_after.map_or("never".to_string(), |n| n.to_string())
    );
    // Not a failure: with the monitor asleep, the session locked or a game
    // in independent flip on that monitor, DWM shows nothing of this window
    // and Windows has no statistics to give.
    if paths.is_empty() {
        println!("no present statistics came back, so no present path was seen");
    }

    // Closing the window: the viewer says so, and presents stop.
    assert!(!viewer.closed());
    // SAFETY: the viewer's live window.
    unsafe { PostMessageW(Some(viewer.window()), WM_CLOSE, WPARAM(0), LPARAM(0)) }.unwrap();
    wait_for("the close", || viewer.closed());
    let frame = pattern.next().unwrap();
    let refused = viewer.present(&Frame {
        video: Some(Video {
            texture: &frame.texture,
            index: 0,
            width: frame.width,
            height: frame.height,
        }),
        cursor: None,
        strip: &strip,
    });
    assert_eq!(
        refused.map(|_| ()).map_err(|err| err.kind()),
        Err(ErrorKind::Closed)
    );
}

// The focus rule keeps a fullscreen window off the screen here, so the
// geometry is checked on a window that is never shown: the same code moves
// it, and Windows answers GetWindowRect for hidden windows the same way.
#[test]
fn fullscreen_covers_the_monitor_exactly_and_comes_back() {
    let _screen = one_at_a_time();
    let Some(mut viewer) = open(Show::Hidden, 640, 360) else {
        return;
    };
    let strip = strip();
    let windowed = window_rect(&viewer);
    let monitor = monitor_rect(&viewer);
    viewer.set_fullscreen(true);
    wait_for("fullscreen", || {
        viewer.fullscreen() && window_rect(&viewer) == monitor
    });
    // A present picks the new size up and resizes the buffers to the
    // whole monitor, which independent flip needs.
    let presented = viewer
        .present(&Frame {
            video: None,
            cursor: None,
            strip: &strip,
        })
        .unwrap();
    assert!(presented.shown);
    assert_eq!(
        viewer.buffer_size(),
        (
            (monitor.right - monitor.left) as u32,
            (monitor.bottom - monitor.top) as u32
        )
    );
    println!(
        "fullscreen: window {:?} on monitor {:?}, buffers {:?}",
        window_rect(&viewer),
        monitor,
        viewer.buffer_size()
    );
    viewer.toggle_fullscreen();
    wait_for("the window's old place", || {
        !viewer.fullscreen() && window_rect(&viewer) == windowed
    });
}

// Small on top: narrower than the window was, above other windows, in the
// bottom right corner of the work area; F11 takes it back to a plain window
// first and its own place after; off again, the window is where it was.
// Never shown, as above.
#[test]
fn small_on_top_tucks_into_a_corner_and_comes_back() {
    let _screen = one_at_a_time();
    let Some(viewer) = open(Show::Hidden, 1280, 720) else {
        return;
    };
    let topmost = |viewer: &Viewer| {
        // SAFETY: a getter on the viewer's live window.
        let ex_style = unsafe { GetWindowLongPtrW(viewer.window(), GWL_EXSTYLE) };
        ex_style & WS_EX_TOPMOST.0 as isize != 0
    };
    // The item's state in the window's system menu: None when it is not there.
    let checked = |viewer: &Viewer| {
        // SAFETY: lookups on the viewer's live window and its own menu.
        let state = unsafe {
            GetMenuState(
                GetSystemMenu(viewer.window(), false),
                SMALL_ON_TOP as u32,
                MF_BYCOMMAND,
            )
        };
        (state != u32::MAX).then_some(state & MF_CHECKED.0 != 0)
    };
    let windowed = window_rect(&viewer);
    assert_eq!(checked(&viewer), Some(false));
    viewer.set_small(true);
    wait_for("small on top", || {
        viewer.small() && window_rect(&viewer) != windowed
    });
    let small = window_rect(&viewer);
    let work = work_rect(&viewer);
    println!("small on top: {small:?} in the work area {work:?}, windowed {windowed:?}");
    assert!(topmost(&viewer));
    assert_eq!(checked(&viewer), Some(true));
    assert!(small.right - small.left < windowed.right - windowed.left);
    assert!(small.right <= work.right && small.bottom <= work.bottom);
    assert!(small.right > work.right - 64 && small.bottom > work.bottom - 64);
    viewer.set_fullscreen(true);
    wait_for("fullscreen from small on top", || {
        viewer.fullscreen() && !viewer.small() && window_rect(&viewer) == monitor_rect(&viewer)
    });
    assert!(!topmost(&viewer));
    viewer.set_fullscreen(false);
    wait_for("the window's old place after fullscreen", || {
        !viewer.fullscreen() && window_rect(&viewer) == windowed
    });
    viewer.set_small(true);
    wait_for("small on top again", || window_rect(&viewer) == small);
    viewer.set_small(false);
    wait_for("the window's old place", || {
        !viewer.small() && window_rect(&viewer) == windowed
    });
    assert!(!topmost(&viewer));
    // The system menu's item, as a click on it sends it.
    let menu_item = || {
        // SAFETY: the viewer's live window; the answer is not needed.
        unsafe {
            SendMessageW(
                viewer.window(),
                WM_SYSCOMMAND,
                Some(WPARAM(SMALL_ON_TOP)),
                Some(LPARAM(0)),
            )
        };
    };
    menu_item();
    wait_for("small on top from the menu", || {
        window_rect(&viewer) == small
    });
    menu_item();
    wait_for("the window's old place from the menu", || {
        !viewer.small() && window_rect(&viewer) == windowed
    });
}

// A still share sends no frames, so the present thread blocks on its
// channel until the window wakes it. Never shown: only messages move it.
#[test]
fn a_resize_fullscreen_and_a_close_wake_the_present_thread() {
    let _screen = one_at_a_time();
    // SAFETY: a constant context; the previous one is not needed.
    unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let (woke, wakes) = mpsc::channel();
    let mut options = Options::new("Booth viewer wake test", 640, 360);
    options.show = Show::Hidden;
    options.wake = Some(Arc::new(move || {
        let _ = woke.send(());
    }));
    let mut viewer = match Viewer::open(&options) {
        Ok(viewer) => viewer,
        Err(err) if err.to_string().contains("no graphics card") => {
            println!("skipped: {err}");
            return;
        }
        Err(err) => panic!("{err}"),
    };
    let strip = strip();
    let still = |viewer: &mut Viewer| {
        viewer
            .present(&Frame {
                video: None,
                cursor: None,
                strip: &strip,
            })
            .unwrap();
    };
    // Once the window's thread is back in its message loop, every wake from
    // what it was handling is in the channel, and these are thrown away so
    // the next step starts clean.
    let settle = |viewer: &Viewer| {
        // SAFETY: the viewer's live window; WM_NULL does nothing.
        unsafe { SendMessageW(viewer.window(), WM_NULL, None, None) };
        while wakes.try_recv().is_ok() {}
    };
    // The flags are stored before the wake that tells of them, so the
    // step's own wake finds them set.
    let woken = |what: &str, done: &dyn Fn() -> bool| {
        let started = Instant::now();
        loop {
            let left = Duration::from_secs(3).saturating_sub(started.elapsed());
            wakes
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("{what} did not wake the present thread"));
            if done() {
                break;
            }
        }
    };
    // Opening records the first size, which is a wake of its own.
    still(&mut viewer);
    settle(&viewer);

    // SAFETY: the viewer's live window. SetWindowPos returns once the
    // window's thread has handled the new size.
    unsafe {
        SetWindowPos(
            viewer.window(),
            None,
            0,
            0,
            700,
            500,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
    }
    .unwrap();
    woken("a resize", &|| viewer.needs_redraw());
    settle(&viewer);
    still(&mut viewer);
    assert!(!viewer.needs_redraw());

    viewer.set_fullscreen(true);
    woken("fullscreen", &|| {
        viewer.fullscreen() && viewer.needs_redraw()
    });
    settle(&viewer);
    still(&mut viewer);
    viewer.set_fullscreen(false);
    woken("leaving fullscreen", &|| !viewer.fullscreen());
    settle(&viewer);

    assert!(!viewer.closed());
    // SAFETY: the viewer's live window.
    unsafe { PostMessageW(Some(viewer.window()), WM_CLOSE, WPARAM(0), LPARAM(0)) }.unwrap();
    woken("the close", &|| viewer.closed());
}

#[test]
fn a_viewer_can_move_to_the_decode_thread() {
    fn movable<T: Send>() {}
    movable::<Viewer>();
}

// Covers a whole monitor for about a second, without taking the focus, and
// reports the present path it saw there. Not run with the rest: it puts a
// window over whatever is on that monitor. Without the focus the taskbar
// may stay on top of it, and then Windows composes it anyway; F11 in the
// example, where the viewer has the focus, is the real check.
//
//   cargo test -p viewer --test window -- --ignored --nocapture
#[test]
#[ignore]
fn fullscreen_on_screen_for_a_second() {
    let _screen = one_at_a_time();
    let Some(mut viewer) = open(Show::NoActivate, 640, 360) else {
        return;
    };
    let mut pattern = Pattern::new(viewer.device(), 640, 360, 240).unwrap();
    let strip = strip();
    let mut show = |viewer: &mut Viewer, frames: usize| {
        let mut paths = Vec::new();
        let mut took = Vec::new();
        for index in 0..frames {
            let frame = pattern.next().unwrap();
            let presented = viewer
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
            took.push(presented.took);
            if let Some(path) = presented.path
                && paths.last().map(|(_, last)| *last) != Some(path)
            {
                paths.push((index, path));
            }
        }
        took.sort();
        (
            paths,
            ms(percentile(&took, 0.5)),
            ms(percentile(&took, 0.95)),
        )
    };
    let windowed = show(&mut viewer, 30);
    viewer.set_fullscreen(true);
    let monitor = monitor_rect(&viewer);
    wait_for("fullscreen", || window_rect(&viewer) == monitor);
    let started = Instant::now();
    let fullscreen = show(&mut viewer, 200);
    let covered = started.elapsed();
    viewer.set_fullscreen(false);
    drop(viewer);
    println!(
        "windowed: paths {:?}, present median {:.3} ms, 95th {:.3} ms",
        windowed.0, windowed.1, windowed.2
    );
    println!(
        "fullscreen for {:.0} ms: paths {:?}, present median {:.3} ms, 95th {:.3} ms",
        ms(covered),
        fullscreen.0,
        fullscreen.1,
        fullscreen.2
    );
}
