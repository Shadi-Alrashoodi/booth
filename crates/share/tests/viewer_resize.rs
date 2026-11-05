// A game can leave too little video memory for the viewer's buffers to take
// a new size. A resize that works again within a moment keeps the watch, the
// window's new size reached and the sharer's pointer shape kept, also one
// the lack of memory kept from the viewer; one that keeps failing ends it
// with the reason. The window is never shown and
// nothing is decoded: the viewer presents its still picture again for each
// new size.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use share::{
    CursorKind, CursorShape, CursorUpdate, Inbox, Line, LinkNumbers, Report, Screen, Show, Watch,
};
use viewer::fault::{self, Shown};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetClientRect, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SetWindowPos,
};
use windows::core::{HSTRING, PCWSTR};

// Resizes fail for every viewer in the process: one test at a time, and
// none left to fail once a test ends, however it ends.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

struct Alone {
    _turn: MutexGuard<'static, ()>,
}

impl Drop for Alone {
    fn drop(&mut self) {
        fault::fail_resizes(0);
        fault::fail_shapes(0);
    }
}

fn alone() -> Alone {
    share::make_process_dpi_aware().unwrap();
    Alone {
        _turn: ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner()),
    }
}

// The window is found by its title, which FindWindowW looks for in every
// process, so another run of these tests at the same time has its own.
fn open(inbox: &Arc<Inbox>, title: &str) -> Option<(Screen, HWND)> {
    let title = format!("{title} {}", std::process::id());
    let watch = Watch {
        title: title.clone(),
        width: 1280,
        height: 720,
        fps: 60,
        vsync: false,
        show: Show::Hidden,
        hevc: false,
        clock: None,
        link: LinkNumbers::default(),
        keep_times: false,
    };
    match Screen::open(&watch, Arc::clone(inbox), &mut |_| {}) {
        Ok(screen) => {
            // SAFETY: a lookup by this test's own title.
            let hwnd =
                unsafe { FindWindowW(PCWSTR::null(), &HSTRING::from(title.as_str())) }.unwrap();
            Some((screen, hwnd))
        }
        Err(why) => {
            println!("skipped: {why}");
            None
        }
    }
}

// The window's new size, as dragging its edge gives, and its client area,
// which the viewer's buffers follow.
fn resize(hwnd: HWND, width: i32, height: i32) -> (u32, u32) {
    // SAFETY: this test's live window. SetWindowPos returns once the
    // window's thread has handled the new size.
    unsafe {
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            width,
            height,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
    }
    .unwrap();
    let mut client = RECT::default();
    // SAFETY: a live window and a live out parameter.
    unsafe { GetClientRect(hwnd, &mut client) }.unwrap();
    (
        (client.right - client.left) as u32,
        (client.bottom - client.top) as u32,
    )
}

// The sharer's pointer moved alone, with a new shape.
fn shaped() -> CursorUpdate {
    CursorUpdate {
        x: 40,
        y: 40,
        visible: true,
        scale: 1.0,
        shape: Some(CursorShape {
            kind: CursorKind::Color,
            width: 32,
            height: 32,
            pitch: 128,
            hotspot_x: 0,
            hotspot_y: 0,
            bytes: vec![0xff; 32 * 32 * 4],
        }),
    }
}

fn tries(lines: &[Line]) -> usize {
    lines
        .iter()
        .filter(|line| matches!(line, Line::Log(text) if text.ends_with("; trying again")))
        .count()
}

#[test]
fn a_resize_that_fails_for_a_moment_keeps_the_watch() {
    let _alone = alone();
    let inbox = Arc::new(Inbox::new().unwrap());
    let Some((screen, hwnd)) = open(&inbox, "Booth viewer resize test") else {
        return;
    };
    let mut seconds = 0;
    let mut client = (0, 0);
    let mut shown = None;
    let mut lines = Vec::new();
    let ran = screen.run(&mut |report| match report {
        Report::Line(line) => lines.push(line),
        Report::Second(_) => {
            seconds += 1;
            if seconds == 1 {
                // The pointer's present is the first to meet the failures,
                // and its shape cannot be uploaded then either.
                fault::fail_resizes(3);
                fault::fail_shapes(1);
                inbox.cursor(shaped(), false);
                client = resize(hwnd, 800, 600);
            } else if seconds == 4 {
                shown = fault::last_shown();
                inbox.stop();
            }
        }
        _ => {}
    });
    ran.expect("a resize that works again within 2 s keeps the watch");
    assert_eq!(
        shown,
        Some(Shown {
            size: client,
            shape: true
        }),
        "{lines:?}"
    );
    assert_eq!(tries(&lines), 1, "{lines:?}");
}

#[test]
fn a_resize_that_keeps_failing_ends_the_watch() {
    let _alone = alone();
    let inbox = Arc::new(Inbox::new().unwrap());
    let Some((screen, hwnd)) = open(&inbox, "Booth viewer resize failing test") else {
        return;
    };
    let mut seconds = 0;
    let mut resized = None;
    let mut lines = Vec::new();
    let ran = screen.run(&mut |report| match report {
        Report::Line(line) => lines.push(line),
        Report::Second(_) => {
            seconds += 1;
            if seconds == 1 {
                fault::fail_resizes(u32::MAX);
                resized = Some((resize(hwnd, 800, 600), Instant::now()));
            } else if seconds == 10 {
                inbox.stop();
            }
        }
        _ => {}
    });
    let ((width, height), at) = resized.unwrap();
    let ended = at.elapsed();
    let why = ran.expect_err("a resize that keeps failing ends the watch");
    assert_eq!(
        why,
        format!(
            "could not resize the viewer's buffers to {width}x{height}: out of memory (E_OUTOFMEMORY)"
        )
    );
    assert!(
        ended >= Duration::from_secs(2),
        "ended {ended:?} after the resize"
    );
    assert_eq!(tries(&lines), 1, "{lines:?}");
}
