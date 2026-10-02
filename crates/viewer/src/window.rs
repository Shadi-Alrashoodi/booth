// The viewer's window and the thread that runs its messages. The window
// thread never touches the swap chain: it records what changed (size, DPI,
// monitor, closed) for the present thread to act on at its next present,
// wakes that thread through Options::wake, and moves the window itself for
// F11. So a present never waits for the window, and dragging the edge never
// lands a resize in the middle of a present.

mod remote;

use std::cell::{Cell, OnceCell};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, TRUE, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, FillRect, GetMonitorInfoW, HBRUSH, HDC, HMONITOR,
    MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint,
    MonitorFromWindow, ScreenToClient,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor,
    GetDpiForWindow, MDT_EFFECTIVE_DPI, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_ESCAPE, VK_F11};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GWL_EXSTYLE, GWL_STYLE,
    GetClientRect, GetCursorPos, GetMessageW, GetWindowLongPtrW, GetWindowPlacement, HTCLIENT,
    HWND_TOP, IDC_ARROW, IDC_HAND, IsWindowVisible, LoadCursorW, MINMAXINFO, MSG, PostMessageW,
    PostQuitMessage, RegisterClassExW, SC_KEYMENU, SIZE_MINIMIZED, SPI_GETCLIENTAREAANIMATION,
    SW_HIDE, SW_SHOWNOACTIVATE, SW_SHOWNORMAL, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetCursor,
    SetWindowLongPtrW, SetWindowPlacement, SetWindowPos, ShowWindow, SystemParametersInfoW,
    TranslateMessage, WA_INACTIVE, WINDOW_EX_STYLE, WINDOW_STYLE, WINDOWPLACEMENT, WM_ACTIVATE,
    WM_ACTIVATEAPP, WM_APP, WM_CLOSE, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_ERASEBKGND,
    WM_GETMINMAXINFO, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR,
    WM_SETFOCUS, WM_SETTINGCHANGE, WM_SIZE, WM_SYSCOMMAND, WM_WINDOWPOSCHANGED, WM_XBUTTONDOWN,
    WM_XBUTTONUP, WNDCLASSEXW, WS_EX_NOACTIVATE, WS_OVERLAPPEDWINDOW, WS_POPUP,
};
use windows::core::{BOOL, HSTRING, w};

use self::remote::{Mouse, button_of, client_point, follow_control, point, pointer};
use crate::control::{self, Capturing, ControlOut, MouseButton, MouseMode};
use crate::error::ViewerError;
use crate::picture::Placement;
use crate::strip::Band;
use crate::{Show, strip};

const CLASS: windows::core::PCWSTR = w!("BoothViewer");
const WM_FULLSCREEN: u32 = WM_APP + 1;
const WM_TEAR_DOWN: u32 = WM_APP + 2;
// Control, or how the mouse goes, changed: capture is worked out again.
const WM_CONTROL: u32 = WM_APP + 3;
// For tests, which never take the focus: this window counts as in front, or
// not, whatever Windows says.
#[cfg(test)]
const WM_PRETEND_FRONT: u32 = WM_APP + 4;
// In UI::Controls, which has nothing else this crate needs.
const WM_MOUSELEAVE: u32 = 0x02A3;

// Shared::mode and Shared::capturing, as one byte each.
const NO_MODE: u8 = 0;
const ABSOLUTE: u8 = 1;
const RELATIVE: u8 = 2;
// Shared::local while the controller's mouse is on no picture.
const NOWHERE: u64 = u64::MAX;

// The ink colour the picture's bars and the strip are drawn in, as a
// COLORREF (0x00BBGGRR). Shown until the first present covers it.
const INK: COLORREF = COLORREF(0x0012_1414);

// The window never opens bigger than this share of the monitor's work area,
// and never bigger than the video at one to one.
const OPEN_SHARE: f64 = 0.8;
// Small enough to tuck into a corner, big enough that some picture shows
// above the strip and the strip's longest words that never drop out end
// before the trace's 132 points: while controlling with the sharer paused,
// "reconnecting direct controlling Control is paused. composed", about 400
// points with their gaps. At 100 percent scaling.
const MIN_CLIENT: (i32, i32) = (560, 180);

// With the setting on, the strip hides in fullscreen until the mouse moves,
// and hides again once the mouse has been still this long.
pub(crate) const STRIP_STAYS: Duration = Duration::from_secs(2);
// No move since the window opened.
const NEVER: u64 = u64::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fullscreen {
    Leave,
    Enter,
    Toggle,
}

// What the window thread tells the present thread. Each field is written by
// one side only, except `changed` and `strip_clicked`, which the window
// thread sets and the present thread clears.
pub(crate) struct Shared {
    // Client width << 32 | height, in physical pixels.
    size: AtomicU64,
    dpi: AtomicU32,
    minimized: AtomicBool,
    closed: AtomicBool,
    fullscreen: AtomicBool,
    // Counts the window landing on another monitor and the display settings
    // changing. Either can change how frames reach the screen while the
    // size stays the same.
    output_changes: AtomicU32,
    // Windows' "Animation effects" switch: on, the strip's trace scrolls.
    scrolling: AtomicBool,
    changed: AtomicBool,
    strip_clicked: AtomicBool,
    // When the mouse last moved over the window, in milliseconds from
    // `opened`, and whether the strip was left out of the last present, so
    // a move then is worth waking the present thread for.
    opened: Instant,
    moved: AtomicU64,
    strip_hidden: AtomicBool,
    wake: Option<Wake>,
    // Remote control (control.rs). `out` is where the controller's input
    // goes, fixed at open. The present thread sets the mode; the window
    // thread switches capture and says here what it switched it to.
    out: Option<Arc<dyn ControlOut>>,
    mode: AtomicU8,
    capturing: AtomicU8,
    // The picture as the last present placed it, which the window thread
    // maps the mouse through.
    placed: Mutex<Option<Placement>>,
    // The controller's mouse in client pixels while it is on the picture in
    // absolute mode, for the present thread to draw the pointer at, as
    // x << 32 | y; NOWHERE otherwise. `local_moved` asks for a present.
    local: AtomicU64,
    local_moved: AtomicBool,
    // The last present had a pointer shape to draw in place of this PC's own
    // pointer, which the window may then hide over the picture.
    has_shape: AtomicBool,
}

pub(crate) type Wake = Arc<dyn Fn() + Send + Sync>;

impl Shared {
    fn new(wake: Option<Wake>, out: Option<Arc<dyn ControlOut>>) -> Shared {
        Shared {
            size: AtomicU64::new(0),
            dpi: AtomicU32::new(96),
            minimized: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            fullscreen: AtomicBool::new(false),
            output_changes: AtomicU32::new(0),
            scrolling: AtomicBool::new(animations_on()),
            changed: AtomicBool::new(true),
            strip_clicked: AtomicBool::new(false),
            opened: Instant::now(),
            moved: AtomicU64::new(NEVER),
            strip_hidden: AtomicBool::new(false),
            wake,
            out,
            mode: AtomicU8::new(NO_MODE),
            capturing: AtomicU8::new(NO_MODE),
            placed: Mutex::new(None),
            local: AtomicU64::new(NOWHERE),
            local_moved: AtomicBool::new(false),
            has_shape: AtomicBool::new(false),
        }
    }

    pub(crate) fn mode(&self) -> Option<MouseMode> {
        mode_of(self.mode.load(Ordering::Acquire))
    }

    // True when it changed. The strip may come back for it: while this PC
    // controls, it never hides.
    pub(crate) fn set_mode(&self, mode: Option<MouseMode>) -> bool {
        let changed = self.mode.swap(code_of(mode), Ordering::AcqRel) != code_of(mode);
        if changed {
            self.mark_changed();
        }
        changed
    }

    // How capture runs now, as the window thread switched it.
    pub(crate) fn capturing(&self) -> Option<MouseMode> {
        mode_of(self.capturing.load(Ordering::Acquire))
    }

    fn set_capturing(&self, how: Option<Capturing>) {
        let mode = how.map(|how| {
            if how.relative {
                MouseMode::Relative
            } else {
                MouseMode::Absolute
            }
        });
        self.capturing.store(code_of(mode), Ordering::Release);
    }

    fn placed(&self) -> Option<Placement> {
        *self.placed.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn set_placed(&self, placed: Option<Placement>) {
        *self.placed.lock().unwrap_or_else(PoisonError::into_inner) = placed;
    }

    pub(crate) fn local(&self) -> Option<(i32, i32)> {
        let packed = self.local.load(Ordering::Acquire);
        (packed != NOWHERE).then_some(((packed >> 32) as u32 as i32, packed as u32 as i32))
    }

    fn set_local(&self, at: Option<(i32, i32)>) {
        let packed = at.map_or(NOWHERE, |(x, y)| {
            u64::from(x as u32) << 32 | u64::from(y as u32)
        });
        if self.local.swap(packed, Ordering::AcqRel) != packed {
            self.local_moved.store(true, Ordering::Release);
            self.wake();
        }
    }

    pub(crate) fn local_moved(&self) -> bool {
        self.local_moved.load(Ordering::Acquire)
    }

    pub(crate) fn take_local_moved(&self) -> bool {
        self.local_moved.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn set_has_shape(&self, has: bool) {
        self.has_shape.store(has, Ordering::Release);
    }

    fn has_shape(&self) -> bool {
        self.has_shape.load(Ordering::Acquire)
    }

    // After the flag it is about is stored, so the woken thread sees it.
    fn wake(&self) {
        if let Some(wake) = &self.wake {
            wake();
        }
    }

    fn mark_changed(&self) {
        self.changed.store(true, Ordering::Release);
        self.wake();
    }

    pub(crate) fn size(&self) -> (u32, u32) {
        let packed = self.size.load(Ordering::Acquire);
        ((packed >> 32) as u32, packed as u32)
    }

    fn set_size(&self, width: u32, height: u32) {
        self.size
            .store((width as u64) << 32 | height as u64, Ordering::Release);
    }

    pub(crate) fn dpi(&self) -> u32 {
        self.dpi.load(Ordering::Acquire)
    }

    pub(crate) fn minimized(&self) -> bool {
        self.minimized.load(Ordering::Acquire)
    }

    pub(crate) fn closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub(crate) fn fullscreen(&self) -> bool {
        self.fullscreen.load(Ordering::Acquire)
    }

    pub(crate) fn output_changes(&self) -> u32 {
        self.output_changes.load(Ordering::Acquire)
    }

    // Woken for it, the present thread presents a still picture again and
    // drops a present path word that may no longer hold.
    fn new_output(&self) {
        self.output_changes.fetch_add(1, Ordering::AcqRel);
        self.mark_changed();
    }

    pub(crate) fn scrolling(&self) -> bool {
        self.scrolling.load(Ordering::Acquire)
    }

    pub(crate) fn changed(&self) -> bool {
        self.changed.load(Ordering::Acquire)
    }

    pub(crate) fn take_changed(&self) -> bool {
        self.changed.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn take_strip_click(&self) -> bool {
        self.strip_clicked.swap(false, Ordering::AcqRel)
    }

    fn mouse_moved(&self) {
        let since = self.opened.elapsed().as_millis();
        self.moved
            .store(u64::try_from(since).unwrap_or(NEVER - 1), Ordering::Release);
        if self.strip_hidden.load(Ordering::Acquire) {
            self.mark_changed();
        }
    }

    fn since_move(&self) -> Option<Duration> {
        let moved = self.moved.load(Ordering::Acquire);
        (moved != NEVER).then(|| {
            self.opened
                .elapsed()
                .saturating_sub(Duration::from_millis(moved))
        })
    }

    // Where the strip goes now, with the setting `hide`. While this PC
    // controls, the strip stays whatever the setting says: its word says
    // this PC's keys go to another, and the mouse moving all the time would
    // keep it over the bottom of the picture, where the sharer's taskbar is.
    pub(crate) fn band(&self, hide: bool) -> Band {
        band(
            hide && self.mode().is_none(),
            self.fullscreen(),
            self.scrolling(),
            self.since_move(),
        )
    }

    pub(crate) fn set_strip_hidden(&self, hidden: bool) {
        self.strip_hidden.store(hidden, Ordering::Release);
    }
}

fn code_of(mode: Option<MouseMode>) -> u8 {
    match mode {
        None => NO_MODE,
        Some(MouseMode::Absolute) => ABSOLUTE,
        Some(MouseMode::Relative) => RELATIVE,
    }
}

fn mode_of(code: u8) -> Option<MouseMode> {
    match code {
        ABSOLUTE => Some(MouseMode::Absolute),
        RELATIVE => Some(MouseMode::Relative),
        _ => None,
    }
}

// With Windows' animations off the strip never hides, so nothing on screen
// comes and goes by itself.
pub(crate) fn band(
    hide: bool,
    fullscreen: bool,
    animations: bool,
    since_move: Option<Duration>,
) -> Band {
    if !(hide && fullscreen && animations) {
        return Band::Below;
    }
    match since_move {
        Some(since) if since < STRIP_STAYS => Band::Over,
        _ => Band::Hidden,
    }
}

pub(crate) struct Window {
    // Kept as a number: HWND holds a raw pointer, which would keep the
    // Viewer on the thread that opened it.
    hwnd: isize,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Window {
    pub(crate) fn open(
        title: &str,
        video: (u32, u32),
        show: Show,
        wake: Option<Wake>,
        out: Option<Arc<dyn ControlOut>>,
    ) -> Result<Window, ViewerError> {
        let shared = Arc::new(Shared::new(wake, out));
        let (opened, wait) = mpsc::channel();
        let title = title.to_string();
        let theirs = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("viewer window".to_string())
            .spawn(move || run(&title, video, show, theirs, opened))
            .map_err(|err| {
                ViewerError::other(format!("could not start the viewer's window thread: {err}"))
            })?;
        match wait.recv() {
            Ok(Ok(hwnd)) => Ok(Window {
                hwnd,
                shared,
                thread: Some(thread),
            }),
            Ok(Err(err)) => {
                let _ = thread.join();
                Err(err)
            }
            Err(_) => {
                let _ = thread.join();
                Err(ViewerError::other(
                    "could not open the viewer window: its thread ended before the window was made",
                ))
            }
        }
    }

    pub(crate) fn hwnd(&self) -> HWND {
        HWND(self.hwnd as *mut _)
    }

    pub(crate) fn shared(&self) -> &Shared {
        &self.shared
    }

    pub(crate) fn fullscreen(&self, change: Fullscreen) {
        let code = match change {
            Fullscreen::Leave => 0,
            Fullscreen::Enter => 1,
            Fullscreen::Toggle => 2,
        };
        // SAFETY: posting to a window this struct keeps alive. A failed post
        // (the queue is full) drops one F11, which a second press repeats.
        let _ = unsafe { PostMessageW(Some(self.hwnd()), WM_FULLSCREEN, WPARAM(code), LPARAM(0)) };
    }

    // After Shared::set_mode: the window thread switches capture to match.
    pub(crate) fn follow_control(&self) {
        // SAFETY: posting to a window this struct keeps alive. A failed post
        // leaves capture as it was until the next change of focus or size;
        // the input crate still sends nothing while another window is in
        // front, and the room nothing once control has ended.
        let _ = unsafe { PostMessageW(Some(self.hwnd()), WM_CONTROL, WPARAM(0), LPARAM(0)) };
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: the window lives until its thread handles this message.
        // If the post fails the thread would never end, so it is not joined.
        let posted = unsafe { PostMessageW(Some(self.hwnd()), WM_TEAR_DOWN, WPARAM(0), LPARAM(0)) };
        if posted.is_ok()
            && let Some(thread) = self.thread.take()
        {
            let _ = thread.join();
        } else if self.shared.capturing().is_some()
            && let Some(out) = &self.shared.out
        {
            // The window's thread will never stop the capture it started.
            out.capture(self.hwnd, None);
        }
    }
}

// Per window thread: each thread runs exactly one viewer window.
thread_local! {
    static SHARED: OnceCell<Arc<Shared>> = const { OnceCell::new() };
    // Where the window was before F11, to go back to.
    static SAVED: Cell<Option<Saved>> = const { Cell::new(None) };
    // Where the last WM_MOUSEMOVE put the pointer. Windows sends the message
    // again with the pointer standing still, when something under it changes.
    static POINTER: Cell<Option<(i16, i16)>> = const { Cell::new(None) };
    // The monitor the window was on when it last moved or changed size, as
    // a number.
    static MONITOR: Cell<Option<isize>> = const { Cell::new(None) };
    static BRUSH: Cell<Option<HBRUSH>> = const { Cell::new(None) };
}

#[derive(Clone, Copy)]
struct Saved {
    placement: WINDOWPLACEMENT,
    style: isize,
}

fn run(
    title: &str,
    video: (u32, u32),
    show: Show,
    shared: Arc<Shared>,
    opened: mpsc::Sender<Result<isize, ViewerError>>,
) {
    // Per-monitor v2 for this thread's windows whatever the process chose,
    // so the swap chain gets physical pixels and WM_DPICHANGED arrives.
    // SAFETY: a constant context; the previous one is not needed.
    unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let _ = SHARED.with(|cell| cell.set(Arc::clone(&shared)));
    // SAFETY: a plain GDI call; the brush is deleted when the thread ends.
    BRUSH.with(|cell| cell.set(Some(unsafe { CreateSolidBrush(INK) })));
    let hwnd = match create(title, video, show, &shared) {
        Ok(hwnd) => hwnd,
        Err(err) => {
            let _ = opened.send(Err(err));
            delete_brush();
            return;
        }
    };
    let _ = opened.send(Ok(hwnd.0 as isize));
    let mut msg = MSG::default();
    // SAFETY: the standard loop on this thread's queue; GetMessageW answers
    // 0 for WM_QUIT and -1 for an error, and both end it.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    delete_brush();
}

fn delete_brush() {
    if let Some(brush) = BRUSH.with(|cell| cell.take()) {
        // SAFETY: made in run() and selected into no DC.
        let _ = unsafe { DeleteObject(brush.into()) };
    }
}

fn register() -> Result<(), ViewerError> {
    static REGISTERED: OnceLock<Result<(), String>> = OnceLock::new();
    REGISTERED
        .get_or_init(|| {
            // SAFETY: plain calls; the class's strings are static.
            unsafe {
                let instance = GetModuleHandleW(None).map_err(|err| err.to_string())?;
                let class = WNDCLASSEXW {
                    cbSize: size_of::<WNDCLASSEXW>() as u32,
                    lpfnWndProc: Some(window_proc),
                    hInstance: instance.into(),
                    hCursor: LoadCursorW(None, IDC_ARROW).map_err(|err| err.to_string())?,
                    lpszClassName: CLASS,
                    ..Default::default()
                };
                if RegisterClassExW(&class) == 0 {
                    return Err(windows::core::Error::from_thread().to_string());
                }
            }
            Ok(())
        })
        .clone()
        .map_err(|err| {
            ViewerError::other(format!("could not register the viewer window class: {err}"))
        })
}

fn create(
    title: &str,
    video: (u32, u32),
    show: Show,
    shared: &Shared,
) -> Result<HWND, ViewerError> {
    register()?;
    let monitor = pointer_monitor();
    let work = monitor_rects(monitor)
        .map(|(_, work)| work)
        .unwrap_or(RECT {
            left: 0,
            top: 0,
            right: 1280,
            bottom: 720,
        });
    let mut dpi = (96, 96);
    // SAFETY: a live monitor handle and two live out parameters.
    let _ = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi.0, &mut dpi.1) };
    let band = strip::band_height(dpi.0);
    let (width, height) = opening_client(
        video,
        (
            (work.right - work.left) as u32,
            (work.bottom - work.top) as u32,
        ),
        band,
        dpi.0,
    );
    let style = WS_OVERLAPPEDWINDOW;
    let ex_style = match show {
        Show::NoActivate | Show::Hidden => WS_EX_NOACTIVATE,
        Show::Activate => WINDOW_EX_STYLE(0),
    };
    let mut frame = RECT {
        left: 0,
        top: 0,
        right: width as i32,
        bottom: height as i32,
    };
    // SAFETY: a live RECT; on failure the frame is left out, which only
    // makes the first size a little smaller.
    let _ = unsafe { AdjustWindowRectExForDpi(&mut frame, style, false, ex_style, dpi.0) };
    let (outer_w, outer_h) = (frame.right - frame.left, frame.bottom - frame.top);
    let x = work.left + ((work.right - work.left) - outer_w).max(0) / 2;
    let y = work.top + ((work.bottom - work.top) - outer_h).max(0) / 2;
    let title = HSTRING::from(title);
    // SAFETY: a registered class, strings alive for the call, no parent.
    let hwnd = unsafe {
        CreateWindowExW(
            ex_style,
            CLASS,
            &title,
            style,
            x,
            y,
            outer_w,
            outer_h,
            None,
            None,
            GetModuleHandleW(None).ok().map(Into::into),
            None,
        )
    }
    .map_err(|err| ViewerError::windows("open the viewer window", &err))?;
    let dark = TRUE;
    // SAFETY: a live window and a BOOL of the size passed. Builds before
    // Windows 10 20H1 refuse it and keep a light title bar.
    let _ = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const BOOL as *const _,
            size_of::<BOOL>() as u32,
        )
    };
    // SAFETY: a live window.
    shared
        .dpi
        .store(unsafe { GetDpiForWindow(hwnd) }.max(96), Ordering::Release);
    record_size(hwnd, shared);
    // SAFETY: a live window. The answer is whether it was visible before.
    unsafe {
        match show {
            Show::Activate => {
                let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
            }
            Show::NoActivate => {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
            Show::Hidden => {}
        }
    }
    Ok(hwnd)
}

// The client size the window opens at: the video scaled down, aspect kept,
// until it and the strip fit the share of the work area, and never scaled up.
pub(crate) fn opening_client(
    video: (u32, u32),
    work: (u32, u32),
    band: u32,
    dpi: u32,
) -> (u32, u32) {
    let room_w = (work.0 as f64 * OPEN_SHARE).floor();
    let room_h = (work.1 as f64 * OPEN_SHARE).floor() - band as f64;
    let (vw, vh) = (video.0.max(1) as f64, video.1.max(1) as f64);
    let scale = (room_w / vw).min(room_h / vh).clamp(0.0, 1.0);
    let (min_w, min_h) = min_client(dpi);
    let width = ((vw * scale).round() as i32).max(min_w) as u32;
    let height = ((vh * scale).round() as i32).max(min_h) as u32;
    (width, height + band)
}

pub(crate) fn min_client(dpi: u32) -> (i32, i32) {
    let dpi = dpi.max(96) as i32;
    (MIN_CLIENT.0 * dpi / 96, MIN_CLIENT.1 * dpi / 96)
}

fn pointer_monitor() -> HMONITOR {
    let mut point = POINT::default();
    // SAFETY: a live out parameter. On the secure desktop the call fails
    // and the point stays at the origin, which is on the primary monitor.
    let _ = unsafe { GetCursorPos(&mut point) };
    // SAFETY: a plain lookup.
    unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTOPRIMARY) }
}

// The monitor's whole rectangle and its work area, in physical pixels.
fn monitor_rects(monitor: HMONITOR) -> Option<(RECT, RECT)> {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: a monitor handle and a MONITORINFO with its size set.
    unsafe { GetMonitorInfoW(monitor, &mut info) }
        .as_bool()
        .then_some((info.rcMonitor, info.rcWork))
}

fn record_size(hwnd: HWND, shared: &Shared) {
    let mut client = RECT::default();
    // SAFETY: a live window and a live out parameter.
    if unsafe { GetClientRect(hwnd, &mut client) }.is_ok() {
        shared.set_size(
            (client.right - client.left).max(0) as u32,
            (client.bottom - client.top).max(0) as u32,
        );
        shared.mark_changed();
    }
}

fn animations_on() -> bool {
    let mut on = TRUE;
    // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL to the pointer.
    let read = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some(&mut on as *mut BOOL as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    // If the call fails, Windows' own default applies, which is on.
    read.is_err() || on.as_bool()
}

// The rows the strip takes when it shows, `y` in client pixels.
fn in_band(shared: &Shared, y: i32) -> bool {
    let (_, height) = shared.size();
    let band = strip::band_height(shared.dpi());
    y >= height as i32 - band as i32 && y < height as i32
}

// A strip the last present left out takes no click and shows no hand.
fn in_strip(shared: &Shared, y: i32) -> bool {
    !shared.strip_hidden.load(Ordering::Acquire) && in_band(shared, y)
}

// A click where the hidden strip would be only brings it back, as a move
// does: the mouse can rest there without moving, and nothing on screen said
// a click would put the panel over a fullscreen share.
fn click(shared: &Shared, y: i32) {
    if in_strip(shared, y) {
        shared.strip_clicked.store(true, Ordering::Release);
        shared.wake();
    } else if in_band(shared, y) {
        shared.mouse_moved();
    }
}

fn set_fullscreen(hwnd: HWND, shared: &Shared, on: bool) {
    if on == shared.fullscreen() {
        return;
    }
    if on {
        enter_fullscreen(hwnd, shared);
    } else {
        leave_fullscreen(hwnd, shared);
    }
    // The new size has woken the present thread already, unless Windows
    // kept the old one; fullscreen itself is news to the caller either way,
    // and a still picture is presented again without the present path word
    // of before.
    shared.mark_changed();
    follow_control(hwnd, shared, false);
}

fn note_monitor(hwnd: HWND, shared: &Shared) {
    // SAFETY: a plain lookup on a live window.
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) }.0 as isize;
    let before = MONITOR.with(|last| last.replace(Some(monitor)));
    if before.is_some_and(|before| before != monitor) {
        shared.new_output();
    }
}

// Borderless over the whole monitor the window is on, which is what lets
// DWM hand the swap chain to the display directly.
fn enter_fullscreen(hwnd: HWND, shared: &Shared) {
    // SAFETY: a plain lookup on a live window.
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let Some((whole, _)) = monitor_rects(monitor) else {
        return;
    };
    let mut placement = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    // SAFETY: a live window and a WINDOWPLACEMENT with its length set.
    if unsafe { GetWindowPlacement(hwnd, &mut placement) }.is_err() {
        return;
    }
    // SAFETY: a live window.
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) };
    SAVED.with(|saved| saved.set(Some(Saved { placement, style })));
    shared.fullscreen.store(true, Ordering::Release);
    let borderless = (style & !(WS_OVERLAPPEDWINDOW.0 as isize)) | WS_POPUP.0 as isize;
    // SAFETY: a live window; the new style is the old one without its frame.
    // SWP_NOACTIVATE: F11 is pressed in an active window already, and a call
    // from the API must not take the focus from a game.
    unsafe {
        SetWindowLongPtrW(hwnd, GWL_STYLE, borderless);
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            whole.left,
            whole.top,
            whole.right - whole.left,
            whole.bottom - whole.top,
            SWP_NOOWNERZORDER | SWP_FRAMECHANGED | SWP_NOACTIVATE,
        );
    }
}

fn leave_fullscreen(hwnd: HWND, shared: &Shared) {
    let Some(mut saved) = SAVED.with(|saved| saved.take()) else {
        return;
    };
    shared.fullscreen.store(false, Ordering::Release);
    // SetWindowPlacement shows the window with the saved command, which
    // would show a hidden one and activate one that is not active.
    // SAFETY: a live window.
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        saved.placement.showCmd = SW_HIDE.0 as u32;
    } else if saved.placement.showCmd == SW_SHOWNORMAL.0 as u32 {
        saved.placement.showCmd = SW_SHOWNOACTIVATE.0 as u32;
    }
    // SAFETY: a live window, its own earlier style and placement.
    unsafe {
        SetWindowLongPtrW(hwnd, GWL_STYLE, saved.style);
        let _ = SetWindowPlacement(hwnd, &saved.placement);
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE
                | SWP_NOSIZE
                | SWP_NOZORDER
                | SWP_NOOWNERZORDER
                | SWP_FRAMECHANGED
                | SWP_NOACTIVATE,
        );
    }
}

extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(shared) = SHARED.with(|cell| cell.get().cloned()) else {
        // SAFETY: the default handling for a message that came before the
        // thread's state was set.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    match msg {
        WM_SIZE => {
            if wparam.0 as u32 == SIZE_MINIMIZED {
                shared.minimized.store(true, Ordering::Release);
            } else {
                shared.minimized.store(false, Ordering::Release);
                record_size(hwnd, &shared);
            }
            follow_control(hwnd, &shared, false);
            LRESULT(0)
        }
        // Capture follows the focus: nothing of this PC's keys or mouse is
        // read for the sharer while another window is in front.
        WM_ACTIVATE => {
            let active =
                (wparam.0 & 0xffff) as u32 != WA_INACTIVE && (wparam.0 >> 16) & 0xffff == 0;
            remote::set_active(active);
            follow_control(hwnd, &shared, false);
            // SAFETY: the default handling, which gives the window the
            // keyboard focus.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_ACTIVATEAPP => {
            if wparam.0 == 0 {
                remote::set_active(false);
            }
            follow_control(hwnd, &shared, false);
            // SAFETY: the default handling.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        // Windows may say this window is active a moment before it is the
        // one in front; the focus comes after.
        WM_SETFOCUS => {
            follow_control(hwnd, &shared, false);
            // SAFETY: the default handling.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_CONTROL => {
            follow_control(hwnd, &shared, false);
            LRESULT(0)
        }
        #[cfg(test)]
        WM_PRETEND_FRONT => {
            remote::pretend_front(wparam.0 != 0);
            follow_control(hwnd, &shared, false);
            LRESULT(0)
        }
        WM_SYSCOMMAND
            if (wparam.0 & 0xfff0) as u32 == SC_KEYMENU
                && control::menu_key_is_the_sharers(shared.mode(), lparam.0) =>
        {
            LRESULT(0)
        }
        WM_DPICHANGED => {
            shared
                .dpi
                .store((wparam.0 as u32 & 0xffff).max(96), Ordering::Release);
            // The band's height follows the DPI even where the client size
            // stays the same.
            shared.mark_changed();
            if shared.fullscreen() {
                // The window already covers its monitor; Windows' suggested
                // rectangle is for a framed window.
                record_size(hwnd, &shared);
            } else {
                // SAFETY: for WM_DPICHANGED, lparam points at the suggested
                // RECT for the new DPI, alive for this message.
                unsafe {
                    let suggested = *(lparam.0 as *const RECT);
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        suggested.left,
                        suggested.top,
                        suggested.right - suggested.left,
                        suggested.bottom - suggested.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
            }
            LRESULT(0)
        }
        WM_GETMINMAXINFO => {
            // SAFETY: getters on a live window.
            let (dpi, style, ex_style) = unsafe {
                (
                    GetDpiForWindow(hwnd).max(96),
                    WINDOW_STYLE(GetWindowLongPtrW(hwnd, GWL_STYLE) as u32),
                    WINDOW_EX_STYLE(GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32),
                )
            };
            let (width, height) = min_client(dpi);
            let mut frame = RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            };
            // SAFETY: a live RECT; on failure the frame is left out. For
            // WM_GETMINMAXINFO, lparam points at a MINMAXINFO the call may
            // change.
            unsafe {
                let _ = AdjustWindowRectExForDpi(&mut frame, style, false, ex_style, dpi);
                let info = &mut *(lparam.0 as *mut MINMAXINFO);
                info.ptMinTrackSize.x = frame.right - frame.left;
                info.ptMinTrackSize.y = frame.bottom - frame.top;
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            // Bit 30 is set on auto-repeat: holding F11 toggles once.
            let repeat = lparam.0 & (1 << 30) != 0;
            let key = wparam.0 as u16;
            if key == VK_F11.0 && !repeat {
                set_fullscreen(hwnd, &shared, !shared.fullscreen());
            } else if key == VK_ESCAPE.0
                && shared.fullscreen()
                && control::escape_leaves_fullscreen(shared.mode())
            {
                set_fullscreen(hwnd, &shared, false);
            }
            LRESULT(0)
        }
        WM_FULLSCREEN => {
            let on = match wparam.0 {
                0 => false,
                1 => true,
                _ => !shared.fullscreen(),
            };
            set_fullscreen(hwnd, &shared, on);
            LRESULT(0)
        }
        // In relative mode the pointer is hidden and held in the window, and
        // a game's click must not open the stats panel here too.
        WM_LBUTTONUP => {
            let at = client_point(lparam);
            let sent = point(hwnd, &shared, Mouse::Button(MouseButton::Left, false), at);
            if !sent && shared.capturing() != Some(MouseMode::Relative) {
                click(&shared, at.1);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN | WM_MBUTTONUP
        | WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let sent = button_of(msg, wparam).is_some_and(|(button, down)| {
                point(
                    hwnd,
                    &shared,
                    Mouse::Button(button, down),
                    client_point(lparam),
                )
            });
            match msg {
                // The X buttons want TRUE for a message handled.
                WM_XBUTTONDOWN | WM_XBUTTONUP if sent => LRESULT(1),
                _ if sent => LRESULT(0),
                // SAFETY: the default handling.
                _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
            }
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            // The wheel's point is on the screen, not in the window.
            let (x, y) = client_point(lparam);
            let mut at = POINT { x, y };
            // SAFETY: a live window and a live point.
            let mapped = unsafe { ScreenToClient(hwnd, &mut at) }.as_bool();
            let delta = i32::from((wparam.0 >> 16) as i16);
            let sent = mapped
                && point(
                    hwnd,
                    &shared,
                    Mouse::Wheel(delta, msg == WM_MOUSEHWHEEL),
                    (at.x, at.y),
                );
            if sent {
                LRESULT(0)
            } else {
                // SAFETY: the default handling.
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }
        WM_MOUSEMOVE => {
            let at = (lparam.0 as i16, (lparam.0 >> 16) as i16);
            if POINTER.with(|pointer| pointer.replace(Some(at))) != Some(at) {
                shared.mouse_moved();
                point(hwnd, &shared, Mouse::Move, client_point(lparam));
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            POINTER.with(|pointer| pointer.set(None));
            remote::mouse_left(&shared);
            LRESULT(0)
        }
        // While control has this PC's pointer, it is hidden: in relative
        // mode all the time, in absolute mode over the picture, where the
        // viewer draws the sharer's pointer in its place.
        WM_SETCURSOR
            if (lparam.0 & 0xffff) as u32 == HTCLIENT
                && match shared.capturing() {
                    Some(MouseMode::Relative) => true,
                    Some(MouseMode::Absolute) => shared.has_shape() && shared.local().is_some(),
                    None => false,
                } =>
        {
            pointer::hide();
            LRESULT(1)
        }
        WM_SETCURSOR if (lparam.0 & 0xffff) as u32 == HTCLIENT => {
            let mut point = POINT::default();
            // SAFETY: live out parameters and a live window.
            let over_strip = unsafe {
                GetCursorPos(&mut point).is_ok()
                    && ScreenToClient(hwnd, &mut point).as_bool()
                    && in_strip(&shared, point.y)
            };
            if over_strip {
                // SAFETY: a system cursor, which is never destroyed.
                unsafe {
                    if let Ok(hand) = LoadCursorW(None, IDC_HAND) {
                        SetCursor(Some(hand));
                    }
                }
                return LRESULT(1);
            }
            // SAFETY: the default handling sets the class cursor.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        // The default handling is what sends WM_SIZE, so it runs too.
        WM_WINDOWPOSCHANGED => {
            note_monitor(hwnd, &shared);
            // SAFETY: the default handling.
            let result = unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            // A pointer held in the window moves with it.
            follow_control(hwnd, &shared, false);
            result
        }
        WM_DISPLAYCHANGE => {
            shared.new_output();
            // SAFETY: the default handling.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_SETTINGCHANGE => {
            let on = animations_on();
            if shared.scrolling.swap(on, Ordering::AcqRel) != on {
                shared.mark_changed();
            }
            // SAFETY: the default handling.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_ERASEBKGND => {
            if let Some(brush) = BRUSH.with(|cell| cell.get()) {
                let mut client = RECT::default();
                // SAFETY: for WM_ERASEBKGND, wparam is the window's DC;
                // the brush lives as long as this thread.
                unsafe {
                    if GetClientRect(hwnd, &mut client).is_ok() {
                        FillRect(HDC(wparam.0 as *mut _), &client, brush);
                    }
                }
            }
            LRESULT(1)
        }
        // Closing the viewer stops watching. The window is only hidden: the
        // swap chain still points at it, and it is destroyed when the
        // Viewer is dropped, after the swap chain.
        WM_CLOSE => {
            shared.closed.store(true, Ordering::Release);
            follow_control(hwnd, &shared, true);
            // SAFETY: a live window.
            let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
            shared.wake();
            LRESULT(0)
        }
        WM_TEAR_DOWN => {
            follow_control(hwnd, &shared, true);
            // SAFETY: this thread's own window.
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_DESTROY => {
            follow_control(hwnd, &shared, true);
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: the default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_at_one_to_one_when_the_video_fits() {
        // 1440p on a 4K monitor at 150 percent: 80 percent of 3840x2088 is
        // 3072x1670, and the strip is 30 px.
        assert_eq!(
            opening_client((2560, 1440), (3840, 2088), 30, 144),
            (2560, 1470)
        );
    }

    #[test]
    fn scales_down_to_fit_keeping_the_aspect() {
        // 80 percent of 1920x1032 is 1536x825, less 20 for the strip.
        let (width, height) = opening_client((2560, 1440), (1920, 1032), 20, 96);
        assert_eq!(height - 20, 805);
        assert_eq!(width, (2560.0f64 * 805.0 / 1440.0).round() as u32);
    }

    #[test]
    fn a_click_on_the_strip_wakes_the_present_thread() {
        let woken = Arc::new(AtomicU32::new(0));
        let count = Arc::clone(&woken);
        let shared = Shared::new(
            Some(Arc::new(move || {
                count.fetch_add(1, Ordering::Relaxed);
            })),
            None,
        );
        shared.set_size(640, 380);
        shared.dpi.store(144, Ordering::Release);
        // At 144 dpi the band is the bottom 30 rows, 350 to 379.
        click(&shared, 349);
        click(&shared, 380);
        assert_eq!(woken.load(Ordering::Relaxed), 0);
        assert!(!shared.take_strip_click());
        click(&shared, 350);
        assert_eq!(woken.load(Ordering::Relaxed), 1);
        assert!(shared.take_strip_click());
        assert!(!shared.take_strip_click());
    }

    #[test]
    fn a_click_where_the_hidden_strip_would_be_only_shows_it() {
        let woken = Arc::new(AtomicU32::new(0));
        let count = Arc::clone(&woken);
        let shared = Shared::new(
            Some(Arc::new(move || {
                count.fetch_add(1, Ordering::Relaxed);
            })),
            None,
        );
        shared.set_size(640, 380);
        shared.dpi.store(144, Ordering::Release);
        shared.fullscreen.store(true, Ordering::Release);
        shared.scrolling.store(true, Ordering::Release);
        shared.take_changed();
        // As the present that left the strip out sets it.
        shared.set_strip_hidden(true);
        assert_eq!(shared.band(true), Band::Hidden);
        assert!(!in_strip(&shared, 360));

        click(&shared, 349);
        assert_eq!(woken.load(Ordering::Relaxed), 0);
        assert_eq!(shared.band(true), Band::Hidden);

        click(&shared, 360);
        assert!(!shared.take_strip_click());
        assert_eq!(woken.load(Ordering::Relaxed), 1);
        assert!(shared.take_changed());
        assert_eq!(shared.band(true), Band::Over);

        // The next present shows it, and then a click there is a strip click.
        shared.set_strip_hidden(false);
        assert!(in_strip(&shared, 360));
        click(&shared, 360);
        assert!(shared.take_strip_click());
    }

    #[test]
    fn never_opens_smaller_than_the_minimum() {
        assert_eq!(opening_client((64, 64), (3840, 2088), 20, 96), (560, 200));
        assert_eq!(opening_client((64, 64), (3840, 2088), 30, 144), (840, 300));
    }

    #[test]
    fn when_the_strip_hides() {
        let still = Some(STRIP_STAYS);
        let just_moved = Some(Duration::from_millis(300));
        assert_eq!(band(true, true, true, still), Band::Hidden);
        assert_eq!(band(true, true, true, None), Band::Hidden);
        assert_eq!(band(true, true, true, just_moved), Band::Over);
        for (hide, fullscreen, animations) in [
            (false, true, true),
            (true, false, true),
            (true, true, false),
        ] {
            assert_eq!(
                band(hide, fullscreen, animations, still),
                Band::Below,
                "{hide} {fullscreen} {animations}"
            );
        }
    }

    // Another monitor or new display settings, with the size the same: the
    // next present has to hear of it, and a still share has to be woken.
    #[test]
    fn a_new_output_wakes_the_present_thread() {
        let woken = Arc::new(AtomicU32::new(0));
        let count = Arc::clone(&woken);
        let shared = Shared::new(
            Some(Arc::new(move || {
                count.fetch_add(1, Ordering::Relaxed);
            })),
            None,
        );
        shared.take_changed();
        let before = shared.output_changes();
        shared.new_output();
        assert_eq!(shared.output_changes(), before + 1);
        assert!(shared.changed());
        assert_eq!(woken.load(Ordering::Relaxed), 1);
    }

    // A move shows the hidden strip at once: the present thread is woken
    // for it. While the strip shows, moves wake nothing.
    #[test]
    fn a_move_wakes_only_a_hidden_strip() {
        let woken = Arc::new(AtomicU32::new(0));
        let count = Arc::clone(&woken);
        let shared = Shared::new(
            Some(Arc::new(move || {
                count.fetch_add(1, Ordering::Relaxed);
            })),
            None,
        );
        shared.fullscreen.store(true, Ordering::Release);
        shared.scrolling.store(true, Ordering::Release);
        assert_eq!(shared.band(true), Band::Hidden);
        shared.take_changed();
        shared.set_strip_hidden(false);
        shared.mouse_moved();
        assert_eq!(woken.load(Ordering::Relaxed), 0);
        assert_eq!(shared.band(true), Band::Over);
        shared.set_strip_hidden(true);
        shared.mouse_moved();
        assert_eq!(woken.load(Ordering::Relaxed), 1);
        assert!(shared.take_changed());
        assert_eq!(shared.band(false), Band::Below);
    }
}
