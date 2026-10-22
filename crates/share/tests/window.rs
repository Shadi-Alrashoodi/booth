// A window share through the sharer, on a plain window this test makes and
// changes the size of: the share starts at the size Windows composes, and
// while the window is dragged to new sizes the pictures keep going out in
// the frame the encoder has, new encoders keep RESIZE_SPACING apart, and the
// window's own size goes out once it stands still. Sizes and times only,
// never the pixels.

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use capture::CursorUpdate;
use encode::Settings;
use share::{
    Audience, Back, Choice, Clock, Kind, Line, PAYLOAD_LAN, RESIZE_SPACING, SWITCH_GAP, Sent,
    Setup, Sharer, WindowId,
};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DispatchMessageW, IsWindow, MSG, PM_REMOVE, PeekMessageW, PostMessageW,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SetWindowPos, TranslateMessage, WINDOW_STYLE,
    WM_CLOSE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_OVERLAPPEDWINDOW, WS_POPUP, WS_VISIBLE,
};
use windows::core::w;

// A window on its own thread, which pumps its messages until it is closed.
struct Window(isize);

impl Window {
    fn with_style(style: WINDOW_STYLE, width: i32, height: i32) -> Window {
        let (sent, got) = mpsc::channel();
        thread::spawn(move || {
            // SAFETY: a system class, constant strings, no parent.
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                    w!("STATIC"),
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

// What the share says and sends, each with when it came.
#[derive(Default)]
struct Watching {
    sent: Vec<(Instant, Sent)>,
    lines: Vec<(Instant, String)>,
}

impl Audience for Watching {
    fn cursor(&mut self, _: CursorUpdate) {}

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
            .filter(|(_, text)| text.contains("starting a new encoder"))
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
}

// On Windows' software encoder a window that grows past 1080p goes on within
// 1920x1080, through a new encoder, never one the software encoder refuses.
#[test]
fn a_window_on_the_software_encoder_keeps_to_1080p() {
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
