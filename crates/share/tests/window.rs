// A window share through the sharer, on a plain window this test makes and
// changes the size of: the share starts at the size Windows composes, and
// while the window is dragged to new sizes the pictures keep going out in
// the frame the encoder has, new encoders keep RESIZE_SPACING apart, and the
// window's own size goes out once it stands still. A pointer update that
// rides a picture says so, after a resume and at a new size too. Sizes and
// times only, never the pixels.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use capture::CursorUpdate;
use encode::Settings;
use share::{
    Audience, Back, Choice, Clock, Codec, Kind, Line, PAYLOAD_LAN, RESIZE_SPACING, SWITCH_GAP,
    Sent, Setup, Sharer, WindowId,
};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DispatchMessageW, GetCursorPos, HWND_TOPMOST, IsWindow, MSG, PM_REMOVE,
    PeekMessageW, PostMessageW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetCursorPos,
    SetWindowPos, SetWindowTextW, TranslateMessage, WINDOW_STYLE, WM_CLOSE, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_OVERLAPPEDWINDOW, WS_POPUP, WS_VISIBLE, WindowFromPoint,
};
use windows::core::{HSTRING, w};

// The tests that put the pointer over their window or keep the GPU encoders
// from opening run alone, the rest side by side: every window here opens at
// the same place, so the others' windows would be under the pointer too, and
// the encoders are kept from opening for every share in this process.
static ALONE: RwLock<()> = RwLock::new(());

// A window on its own thread, which pumps its messages until it is closed.
struct Window(isize);

impl Window {
    fn with_style(style: WINDOW_STYLE, width: i32, height: i32) -> Window {
        Window::made(style, width, height, false)
    }

    // One Windows snaps to a side of its monitor: not a tool window, and of a
    // class that leaves a double click on its edge to Windows, which STATIC
    // does not.
    fn snappable(width: i32, height: i32) -> Window {
        Window::made(WS_OVERLAPPEDWINDOW, width, height, true)
    }

    fn made(style: WINDOW_STYLE, width: i32, height: i32, snaps: bool) -> Window {
        let (sent, got) = mpsc::channel();
        thread::spawn(move || {
            let (ex, class) = if snaps {
                (WS_EX_NOACTIVATE, w!("BUTTON"))
            } else {
                (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, w!("STATIC"))
            };
            // SAFETY: a system class, constant strings, no parent.
            let hwnd = unsafe {
                CreateWindowExW(
                    ex,
                    class,
                    w!("Booth window share test"),
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

    fn id(&self) -> WindowId {
        WindowId::of(self.0).unwrap()
    }
}

fn resize(hwnd: isize, width: i32, height: i32) {
    // SAFETY: a window of this process; the move is sent to its thread.
    let _ = unsafe {
        SetWindowPos(
            HWND(hwnd as *mut _),
            None,
            0,
            0,
            width,
            height,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
    };
}

impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: posted; the window's own thread destroys it.
        let _ =
            unsafe { PostMessageW(Some(HWND(self.0 as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0)) };
    }
}

// What the share says and sends, each with when it came, and each pointer
// update with whether it came with a picture.
#[derive(Default)]
struct Watching {
    sent: Vec<(Instant, Sent)>,
    lines: Vec<(Instant, String)>,
    cursors: Vec<(CursorUpdate, bool)>,
    // When each pointer update came.
    cursors_at: Vec<Instant>,
}

impl Audience for Watching {
    fn cursor(&mut self, update: CursorUpdate, with_picture: bool) {
        self.cursors.push((update, with_picture));
        self.cursors_at.push(Instant::now());
    }

    fn back(&mut self, _: &mut Vec<Back>) {}

    fn sent(&mut self, frame: &Sent) {
        self.sent.push((Instant::now(), *frame));
    }

    fn line(&mut self, line: Line) {
        let (Line::Say(text) | Line::Log(text)) = line;
        self.lines.push((Instant::now(), text));
    }
}

impl Watching {
    // When each new encoder for a new size opened.
    fn new_sizes(&self) -> Vec<Instant> {
        self.lines
            .iter()
            .filter(|(_, text)| text.contains("a new encoder opened"))
            .map(|(at, _)| *at)
            .collect()
    }
}

fn adapter() -> Option<capture::Adapter> {
    capture::make_process_dpi_aware().unwrap();
    if capture::monitors().unwrap().is_empty() {
        println!("skipped: no monitor is attached to the desktop");
        return None;
    }
    let adapters = capture::adapters().unwrap();
    let Some(adapter) = adapters.into_iter().next() else {
        println!("skipped: this PC has no hardware graphics adapter");
        return None;
    };
    Some(adapter)
}

fn open(window: &Window, adapter: capture::Adapter) -> Sharer {
    open_with(window, adapter, None, 120)
}

fn open_with(
    window: &Window,
    adapter: capture::Adapter,
    encoder: Option<Kind>,
    fps: u32,
) -> Sharer {
    let setup = Setup {
        choice: Choice::Window {
            window: window.id(),
            adapter,
        },
        fps,
        settings: Settings::default(),
        encoder,
        codec: None,
        takes_hevc: true,
        payload: PAYLOAD_LAN,
        spread: false,
        clock: Clock::starting(Instant::now()),
        keep_times: false,
    };
    Sharer::open(setup, |_: &[u8]| {}, &mut |_| {}).unwrap_or_else(|err| panic!("{err}"))
}

fn run(sharer: &mut Sharer, watching: &mut Watching, for_: Duration) {
    let until = Instant::now() + for_;
    while Instant::now() < until {
        sharer.next(watching).unwrap_or_else(|err| panic!("{err}"));
    }
}

// With a title bar and borders the share opens at the size of the pictures,
// smaller than the window's rectangle, so no second encoder comes once one
// could.
#[test]
fn a_window_share_opens_at_the_size_of_its_pictures() {
    let _turn = ALONE.read().unwrap_or_else(|p| p.into_inner());
    let Some(adapter) = adapter() else { return };
    let window = Window::with_style(WS_OVERLAPPEDWINDOW, 640, 360);
    let mut sharer = open(&window, adapter);
    let opened = sharer.size();
    assert!(opened.0 < 640 && opened.1 < 360, "{opened:?}");
    let mut watching = Watching::default();
    run(
        &mut sharer,
        &mut watching,
        RESIZE_SPACING + Duration::from_millis(500),
    );
    println!(
        "a 640x360 window with a title bar shares at {opened:?} with {}",
        sharer.encoder_name()
    );
    assert!(!watching.sent.is_empty(), "nothing went out");
    assert!(watching.sent[0].1.idr);
    assert_eq!(sharer.size(), opened);
    assert!(watching.new_sizes().is_empty(), "{:?}", watching.lines);
    assert_eq!(sharer.numbers().resized, 0);
    // The pointer goes on its own: its first update goes whether it is over
    // the window or not, and a shown one is on the picture. The first rides
    // the first picture, which the fps cap sends at once, so it is marked as
    // that picture's.
    let (first, with_picture) = watching.cursors.first().expect("a pointer update");
    assert!(*with_picture, "{first:?} went alone");
    for (update, _) in watching.cursors.iter().filter(|(update, _)| update.visible) {
        assert!(
            update.x < opened.0 as i32 && update.y < opened.1 as i32,
            "{update:?}"
        );
    }
}

// Paused by hand and resumed, a share's source opens again and its first
// pointer update goes as before, on the first picture, marked as its own.
#[test]
fn a_resumed_share_marks_its_first_pointer() {
    let _turn = ALONE.read().unwrap_or_else(|p| p.into_inner());
    let Some(adapter) = adapter() else { return };
    let window = Window::with_style(WS_POPUP, 640, 360);
    let mut sharer = open(&window, adapter);
    let mut watching = Watching::default();
    run(&mut sharer, &mut watching, Duration::from_millis(300));
    sharer.pause();
    let before = watching.cursors.len();
    assert!(
        sharer
            .resume(&mut |_| {})
            .unwrap_or_else(|err| panic!("{err}"))
    );
    run(&mut sharer, &mut watching, Duration::from_millis(300));
    let (first, with_picture) = watching
        .cursors
        .get(before)
        .expect("a pointer update after the resume");
    assert!(*with_picture, "{first:?} went alone");
}

// Puts the pointer back where it was when dropped.
struct PointerBack(POINT);

impl Drop for PointerBack {
    fn drop(&mut self) {
        // SAFETY: a plain call.
        let _ = unsafe { SetCursorPos(self.0.x, self.0.y) };
    }
}

// The pointer over `window`, until what this gives drops; None when another
// window is in front of it there. A window made without being activated can
// open under the active one, so it is put on top, and the windows of the
// tests that ran before close on their own threads, so they are given a
// moment.
fn pointer_over(window: &Window) -> Option<PointerBack> {
    let mut was = POINT::default();
    // SAFETY: a live POINT.
    let _ = unsafe { GetCursorPos(&mut was) };
    let back = PointerBack(was);
    // SAFETY: a window of this process; the change is sent to its thread.
    let _ = unsafe {
        SetWindowPos(
            HWND(window.0 as *mut _),
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
    };
    // The helper puts its windows at 120,120 on the desktop.
    let over = POINT { x: 440, y: 300 };
    let start = Instant::now();
    // SAFETY: plain calls.
    while unsafe {
        let _ = SetCursorPos(over.x, over.y);
        WindowFromPoint(over) != HWND(window.0 as *mut _)
    } {
        if start.elapsed() > Duration::from_secs(1) {
            println!("skipped: another window is over the test's own");
            return None;
        }
        thread::sleep(Duration::from_millis(20));
    }
    Some(back)
}

// A window that grew past the share's frame goes out fitted into it, and
// once it may, at its own size through a new encoder, whether it stands
// still by then or keeps changing: the pointer resting over it goes at its
// place on that picture, with it, not alone a moment later.
#[test]
fn a_resized_windows_pointer_goes_with_its_own_size() {
    let _turn = ALONE.write().unwrap_or_else(|p| p.into_inner());
    let Some(adapter) = adapter() else { return };
    // One window for both: a second one made later can open under others.
    let window = Window::with_style(WS_POPUP, 640, 360);
    let Some(_back) = pointer_over(&window) else {
        return;
    };
    for changing in [false, true] {
        resize(window.0, 640, 360);
        thread::sleep(Duration::from_millis(100));
        let stop = Arc::new(AtomicBool::new(false));
        let changer = changing.then(|| {
            let (stop, hwnd) = (Arc::clone(&stop), window.0);
            thread::spawn(move || {
                let mut count = 0u32;
                while !stop.load(Ordering::Relaxed) {
                    let text = HSTRING::from(count.to_string());
                    // SAFETY: sent to the window's own thread, which paints it.
                    let _ = unsafe { SetWindowTextW(HWND(hwnd as *mut _), &text) };
                    count += 1;
                    thread::sleep(Duration::from_millis(5));
                }
            })
        });
        let mut sharer = open(&window, adapter.clone());
        let mut watching = Watching::default();
        run(&mut sharer, &mut watching, Duration::from_millis(300));
        resize(window.0, 1280, 720);
        run(
            &mut sharer,
            &mut watching,
            RESIZE_SPACING + Duration::from_millis(500),
        );
        stop.store(true, Ordering::Relaxed);
        if let Some(changer) = changer {
            changer.join().unwrap();
        }
        assert_eq!(sharer.size(), (1280, 720), "{:?}", watching.lines);
        // The newest update when the new encoder's first picture goes out
        // came after that encoder opened.
        let opened = watching.new_sizes()[0];
        let out = watching
            .sent
            .iter()
            .map(|(at, _)| *at)
            .find(|&at| at > opened)
            .expect("nothing went out through the new encoder");
        let before = watching.cursors_at.iter().filter(|&&at| at <= out).count();
        let (update, with_picture) = &watching.cursors[before.max(1) - 1];
        assert!(
            watching.cursors_at[before.max(1) - 1] > opened
                && *with_picture
                && update.visible
                && (update.scale - 1.0).abs() < 0.01,
            "changing {changing}: {update:?} with the picture {with_picture}"
        );
    }
}

// Takes back the encoders' faults however the test ends.
struct Faults;

impl Drop for Faults {
    fn drop(&mut self) {
        encode::fault::clear();
    }
}

// A window grown while no new encoder opens for its size goes on fitted into
// the share's frame, and so does the pointer over it, with a picture or
// alone: the update made for the picture at the window's own size, which
// never goes out, does not go with the fitted one, and the ones after it are
// placed on the fitted picture.
#[test]
fn a_pointer_goes_only_with_its_own_picture() {
    let _turn = ALONE.write().unwrap_or_else(|p| p.into_inner());
    let Some(adapter) = adapter() else { return };
    let window = Window::with_style(WS_POPUP, 640, 360);
    let Some(_back) = pointer_over(&window) else {
        return;
    };
    let mut sharer = open(&window, adapter);
    if sharer.kind() == Some(Kind::MfSoftware) {
        println!("skipped: the share is on the software encoder");
        return;
    }
    let mut watching = Watching::default();
    run(&mut sharer, &mut watching, Duration::from_millis(300));
    let hevc = sharer.codec() == Some(Codec::Hevc);
    let _faults = Faults;
    encode::fault::refuse_gpu(400);
    resize(window.0, 1280, 720);
    run(
        &mut sharer,
        &mut watching,
        RESIZE_SPACING + Duration::from_millis(500),
    );
    assert_eq!(sharer.size(), (640, 360), "{:?}", watching.lines);
    assert!(
        watching
            .lines
            .iter()
            .any(|(_, text)| text.starts_with("no new encoder")),
        "{:?}",
        watching.lines
    );
    // No line says one opened, or that the share went on in H.264.
    assert!(
        !watching.lines.iter().any(|(_, text)| {
            text.starts_with("the picture went from") || text.contains("going on in H.264")
        }),
        "{:?}",
        watching.lines
    );
    // In HEVC, that HEVC did not open and H.264 is tried.
    assert!(
        !hevc
            || watching.lines.iter().any(|(_, text)| {
                text.starts_with("HEVC did not open at 1280x720: ")
                    && text.ends_with("; trying H.264")
            }),
        "{:?}",
        watching.lines
    );
    // Scaled by half into the frame from the first fitted picture on.
    let fitted = watching
        .cursors
        .iter()
        .position(|(update, with_picture)| *with_picture && update.scale < 0.9)
        .expect("no pointer update with a fitted picture");
    let after: Vec<(f32, bool)> = watching.cursors[fitted..]
        .iter()
        .map(|(update, with_picture)| (update.scale, *with_picture))
        .collect();
    assert!(after.iter().all(|&(scale, _)| scale < 0.9), "{after:?}");
}

// A window in HEVC under a 4K limit grown past H.264's box, at a size where
// HEVC does not open, or is not offered: capture keeps the window within the
// box, it goes on fitted into the share's frame, and its picture within the
// box goes out through a new encoder in HEVC. The log says HEVC did not open
// there, or was not offered, that the window goes on fitted, and that the new
// encoder opened in HEVC; it never says "going on in H.264".
#[test]
fn a_window_past_h264s_box_without_hevc_goes_on_fitted() {
    let _turn = ALONE.write().unwrap_or_else(|p| p.into_inner());
    let Some(adapter) = adapter() else { return };
    for offered in [true, false] {
        let window = Window::with_style(WS_POPUP, 640, 360);
        let mut sharer = open(&window, adapter.clone());
        if sharer.kind() != Some(Kind::Nvenc) || sharer.codec() != Some(Codec::Hevc) {
            println!("skipped: no NVENC here to share in HEVC with");
            return;
        }
        let four_k = capture::Options {
            max_width: 4096,
            max_height: 2160,
            max_fps: 120,
        };
        sharer
            .set_limit(four_k, &mut |_| {})
            .unwrap_or_else(|err| panic!("{err}"));
        let mut watching = Watching::default();
        run(&mut sharer, &mut watching, Duration::from_millis(300));
        let _faults = Faults;
        if offered {
            encode::fault::refuse_gpu(1440);
        } else {
            encode::fault::refuse_hevc_offer(1440);
        }
        resize(window.0, 2560, 1600);
        run(
            &mut sharer,
            &mut watching,
            RESIZE_SPACING + Duration::from_millis(800),
        );
        let said = |text: &str| watching.lines.iter().any(|(_, line)| line.contains(text));
        let refused = if offered {
            "HEVC did not open at 2560x1600"
        } else {
            "no HEVC at 2560x1600"
        };
        assert!(said(refused), "{:?}", watching.lines);
        assert!(
            said("goes on fitted into 640x360 until a new encoder opens"),
            "{:?}",
            watching.lines
        );
        assert!(!said("going on in H.264"), "{:?}", watching.lines);
        assert!(
            !said("the picture went from 640x360 to 2560x1600"),
            "{:?}",
            watching.lines
        );
        let (width, height) = sharer.size();
        assert_eq!(
            (height, sharer.codec()),
            (1440, Some(Codec::Hevc)),
            "{:?}",
            watching.lines
        );
        assert!(
            said(&format!(
                "the picture went from 640x360 to {width}x{height}: a new encoder opened in HEVC"
            )),
            "{:?}",
            watching.lines
        );
    }
}

// On Windows' software encoder a window that grows past 1080p goes on within
// 1920x1080, through a new encoder, never one the software encoder refuses.
#[test]
fn a_window_on_the_software_encoder_keeps_to_1080p() {
    let _turn = ALONE.read().unwrap_or_else(|p| p.into_inner());
    let Some(adapter) = adapter() else { return };
    let window = Window::with_style(WS_POPUP, 800, 600);
    let mut sharer = open_with(&window, adapter, Some(Kind::MfSoftware), 60);
    let mut watching = Watching::default();
    run(&mut sharer, &mut watching, Duration::from_millis(300));
    resize(window.0, 2560, 1440);
    run(
        &mut sharer,
        &mut watching,
        RESIZE_SPACING + Duration::from_millis(800),
    );
    let (width, height) = sharer.size();
    println!("a 2560x1440 window on the software encoder shares at {width}x{height}");
    assert!(width <= 1920 && height <= 1080, "{:?}", watching.lines);
    assert!(sharer.numbers().resized > 0, "{:?}", watching.lines);
    assert!(
        !watching
            .lines
            .iter()
            .any(|(_, text)| text.starts_with("no new encoder")),
        "{:?}",
        watching.lines
    );
}

// A window dragged bigger for 5 s, a step every 10 ms, then still: frames
// go out all the while, new encoders open at most once every
// RESIZE_SPACING, never four within SWITCH_GAP, and the window's last size
// goes out once it stands still.
#[test]
fn a_dragged_window_keeps_its_encoders_apart() {
    let _turn = ALONE.read().unwrap_or_else(|p| p.into_inner());
    let Some(adapter) = adapter() else { return };
    let window = Window::with_style(WS_POPUP, 640, 360);
    let mut sharer = open(&window, adapter);
    assert_eq!(sharer.size(), (640, 360));
    let mut watching = Watching::default();
    run(&mut sharer, &mut watching, Duration::from_millis(300));
    let hwnd = window.0;
    let drag = thread::spawn(move || {
        for step in 0..500 {
            resize(hwnd, 640 + step, 360 + step / 2);
            thread::sleep(Duration::from_millis(10));
        }
    });
    let started = Instant::now();
    while !drag.is_finished() {
        sharer
            .next(&mut watching)
            .unwrap_or_else(|err| panic!("{err}"));
    }
    drag.join().unwrap();
    let dragged = started.elapsed();
    let during = watching
        .sent
        .iter()
        .filter(|(at, _)| *at >= started)
        .count();
    run(
        &mut sharer,
        &mut watching,
        RESIZE_SPACING + Duration::from_millis(500),
    );
    let new_sizes = watching.new_sizes();
    let gaps: Vec<u128> = new_sizes
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).as_millis())
        .collect();
    println!(
        "{during} frames out in the {:.1} s drag, new encoders {:?} ms apart, the share at {:?}",
        dragged.as_secs_f64(),
        gaps,
        sharer.size()
    );
    assert_eq!(sharer.size(), (1140, 610), "{:?}", watching.lines);
    // The share's thread notes the line a moment after the encoder opens,
    // never before.
    let shortest = RESIZE_SPACING - Duration::from_millis(20);
    assert!(
        new_sizes
            .windows(2)
            .all(|pair| pair[1] - pair[0] >= shortest),
        "{gaps:?}"
    );
    assert!(
        new_sizes
            .windows(4)
            .all(|four| four[3] - four[0] >= SWITCH_GAP),
        "{gaps:?}"
    );
    assert!(
        during as f64 >= dragged.as_secs_f64() * 30.0,
        "only {during} frames in {dragged:?}"
    );
}

// Paused by hand, minimized, and resumed while still minimized, a share opens
// at the size it had: not its caption's, nor what the window's placement
// says it comes back at, which counts the invisible borders the pictures
// leave out and, for a window snapped to its monitor's full height as a
// double click on its top edge does, is where it was before the snap.
#[test]
fn a_share_resumed_while_minimized_keeps_its_size() {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, HTTOP, IsIconic, SW_MINIMIZE, SendMessageW, ShowWindowAsync,
        WM_NCLBUTTONDBLCLK,
    };
    let _turn = ALONE.read().unwrap_or_else(|p| p.into_inner());
    let Some(adapter) = adapter() else { return };
    let window = Window::snappable(640, 360);
    let hwnd = HWND(window.0 as *mut _);
    let rect = || {
        let mut rect = RECT::default();
        // SAFETY: a live RECT; a gone window fails.
        let _ = unsafe { GetWindowRect(hwnd, &mut rect) };
        rect
    };
    let before = rect();
    let edge = (before.top + 2) << 16 | (((before.left + before.right) / 2) & 0xffff);
    // SAFETY: returns once the window's own thread has handled it.
    unsafe {
        SendMessageW(
            hwnd,
            WM_NCLBUTTONDBLCLK,
            Some(WPARAM(HTTOP as usize)),
            Some(LPARAM(edge as isize)),
        )
    };
    let snapped = rect() != before;
    thread::sleep(Duration::from_millis(300));
    let mut sharer = open(&window, adapter);
    let mut watching = Watching::default();
    run(&mut sharer, &mut watching, Duration::from_millis(300));
    let size = sharer.size();
    sharer.pause();
    // SAFETY: posted to the window's own thread.
    let _ = unsafe { ShowWindowAsync(hwnd, SW_MINIMIZE) };
    let start = Instant::now();
    // SAFETY: a plain query on a window of this process.
    while !unsafe { IsIconic(hwnd) }.as_bool() {
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "the window did not minimize"
        );
        thread::sleep(Duration::from_millis(10));
    }
    thread::sleep(Duration::from_millis(300));
    assert!(
        sharer
            .resume(&mut |_| {})
            .unwrap_or_else(|err| panic!("{err}"))
    );
    println!(
        "snapped {snapped}: shared at {size:?}, resumed minimized at {:?}",
        sharer.size()
    );
    assert_eq!(sharer.size(), size);
}
