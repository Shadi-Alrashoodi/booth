// The region picker: every monitor dimmed under a window of its own, and the
// rectangle dragged out on one of them is what a region share sends. Letting
// go takes it when it is at least share::SMALLEST_REGION each way; Esc, the
// right button or switching to another program takes nothing. The windows
// are gone before the share opens, so they are never in it.
//
// Each window is one tone at one alpha with the rectangle cut out of it by a
// colour key, so what is inside shows as it is, and the edge and the words
// stay readable over any picture.

use std::cell::RefCell;
use std::io;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::{mem, ptr};

use eframe::egui::Color32;
use share::{Monitor, MonitorId, Region, SMALLEST_REGION};
use windows_sys::Win32::Foundation::{
    COLORREF, ERROR_CLASS_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    AddFontMemResourceEx, BeginPaint, BitBlt, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS,
    CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreateSolidBrush, DEFAULT_CHARSET,
    DEFAULT_PITCH, DT_CALCRECT, DT_NOPREFIX, DT_SINGLELINE, DeleteDC, DeleteObject, DrawTextW,
    EndPaint, FW_NORMAL, FillRect, HDC, HFONT, InvalidateRect, OUT_DEFAULT_PRECIS, PAINTSTRUCT,
    SRCCOPY, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetThreadDpiAwarenessContext,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VK_ESCAPE};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    IDC_CROSS, LWA_ALPHA, LWA_COLORKEY, LoadCursorW, MSG, PostQuitMessage, RegisterClassExW,
    SW_SHOW, SetCursor, SetForegroundWindow, SetLayeredWindowAttributes, ShowWindow,
    WM_ACTIVATEAPP, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_PAINT, WM_RBUTTONUP, WM_SETCURSOR, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};

use crate::messages;
use crate::theme::{self, AMBER, BAD, CHALK, PANEL, WINDOW};

const CLASS: &str = "BoothRegion";
// What the windows let through: never drawn as anything else.
const KEY: COLORREF = 0x00FF_00FF;
// How much of each window covers what is under it.
const ALPHA: u8 = 168;
// At 100 percent: the edge around the rectangle, the words' size, and the
// space around them.
const EDGE: i32 = 2;
const TEXT_PX: i32 = 14;
const PAD: i32 = 8;

// The monitor and the region of it, in its own upright pixels.
pub struct Picked {
    pub monitor: MonitorId,
    pub refresh_hz: f64,
    pub region: Region,
}

// Opens the picker on its own thread at once. The answer comes once, None
// when nothing was taken or the error when it could not open, and `done` is
// called then, to wake the panel.
pub fn pick(
    monitors: Vec<Monitor>,
    done: impl FnOnce() + Send + 'static,
) -> io::Result<Receiver<Result<Option<Picked>, String>>> {
    let (answer, answered) = mpsc::channel();
    thread::Builder::new()
        .name("region picker".into())
        .spawn(move || {
            let picked = run(&monitors).map_err(|err| err.to_string());
            let _ = answer.send(picked);
            done();
        })?;
    Ok(answered)
}

struct Picker {
    windows: Vec<(HWND, usize)>,
    monitors: Vec<Monitor>,
    // The window dragged on, where the drag began and where it is now.
    drag: Option<(HWND, POINT, POINT)>,
    // A rectangle let go of that is too small, still drawn with why.
    small: Option<HWND>,
    picked: Option<Picked>,
}

thread_local! {
    static PICKER: RefCell<Option<Picker>> = const { RefCell::new(None) };
}

fn run(monitors: &[Monitor]) -> io::Result<Option<Picked>> {
    // No window would ever end the loop.
    if monitors.is_empty() {
        return Ok(None);
    }
    // Physical pixels whatever the process chose, as the region needs.
    // SAFETY: a constant context; the previous one is not needed.
    unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    register()?;
    add_font();
    PICKER.with(|picker| {
        *picker.borrow_mut() = Some(Picker {
            windows: Vec::new(),
            monitors: monitors.to_vec(),
            drag: None,
            small: None,
            picked: None,
        })
    });
    for (index, monitor) in monitors.iter().enumerate() {
        match open_window(monitor) {
            Ok(hwnd) => PICKER.with(|picker| {
                if let Some(picker) = picker.borrow_mut().as_mut() {
                    picker.windows.push((hwnd, index));
                }
            }),
            Err(err) => {
                close_all();
                return Err(err);
            }
        }
    }
    let first = PICKER.with(|picker| {
        picker
            .borrow()
            .as_ref()
            .and_then(|picker| picker.windows.first().map(|(hwnd, _)| *hwnd))
    });
    if let Some(hwnd) = first {
        // SAFETY: a live window of this thread; the panel was in front, so
        // Windows lets it take the focus for Esc.
        unsafe { SetForegroundWindow(hwnd) };
    }
    // SAFETY: the standard loop on this thread's queue, which ends with
    // PostQuitMessage once something was taken or not.
    unsafe {
        let mut message: MSG = mem::zeroed();
        while GetMessageW(&mut message, ptr::null_mut(), 0, 0) > 0 {
            DispatchMessageW(&message);
        }
    }
    let picked = PICKER.with(|picker| picker.borrow_mut().as_mut().and_then(|p| p.picked.take()));
    close_all();
    Ok(picked)
}

fn close_all() {
    let windows = PICKER.with(|picker| {
        picker
            .borrow_mut()
            .take()
            .map(|picker| picker.windows)
            .unwrap_or_default()
    });
    for (hwnd, _) in windows {
        // SAFETY: a window this thread made and has not destroyed.
        unsafe { DestroyWindow(hwnd) };
    }
}

fn register() -> io::Result<()> {
    let class: Vec<u16> = CLASS.encode_utf16().chain([0]).collect();
    // SAFETY: a null name asks for this exe's own module handle.
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    let info = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: class.as_ptr(),
        ..WNDCLASSEXW::default()
    };
    // SAFETY: `info` carries its own size, and the class name is zero
    // terminated and outlives the call, which copies it.
    if unsafe { RegisterClassExW(&info) } == 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(ERROR_CLASS_ALREADY_EXISTS as i32) {
            return Err(io::Error::new(
                err.kind(),
                format!("could not register the region picker's window class: {err}"),
            ));
        }
    }
    Ok(())
}

fn open_window(monitor: &Monitor) -> io::Result<HWND> {
    let class: Vec<u16> = CLASS.encode_utf16().chain([0]).collect();
    // SAFETY: as in register.
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    // SAFETY: the class was registered with this module, the name is zero
    // terminated, and every other pointer is null, which CreateWindowExW
    // allows for a top-level window without a title.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class.as_ptr(),
            ptr::null(),
            WS_POPUP,
            monitor.left,
            monitor.top,
            monitor.width as i32,
            monitor.height as i32,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null(),
        )
    };
    if hwnd.is_null() {
        let err = io::Error::last_os_error();
        return Err(io::Error::new(
            err.kind(),
            format!(
                "could not open the region picker on {}: {err}",
                monitor.name
            ),
        ));
    }
    // SAFETY: a live layered window of this thread.
    unsafe {
        SetLayeredWindowAttributes(hwnd, KEY, ALPHA, LWA_COLORKEY | LWA_ALPHA);
        ShowWindow(hwnd, SW_SHOW);
    }
    Ok(hwnd)
}

// IBM Plex Sans as the panel has it, from the bytes the app carries, for
// this process alone. Added again on a second pick, which costs nothing.
fn add_font() {
    let bytes = theme::SANS_REGULAR;
    let mut count = 0;
    // SAFETY: the font's bytes live for the whole run, and Windows copies
    // them; the count is a live out parameter.
    unsafe {
        AddFontMemResourceEx(
            bytes.as_ptr() as *const _,
            bytes.len() as u32,
            ptr::null(),
            &raw mut count,
        )
    };
}

// At `px` pixels high. A null answer leaves the system font, which is only
// plainer.
fn font(px: i32) -> HFONT {
    let name: Vec<u16> = "IBM Plex Sans".encode_utf16().chain([0]).collect();
    // SAFETY: a zero-terminated face name that outlives the call.
    unsafe {
        CreateFontW(
            -px,
            0,
            0,
            0,
            FW_NORMAL as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            DEFAULT_PITCH as u32,
            name.as_ptr(),
        )
    }
}

fn colour(colour: Color32) -> COLORREF {
    u32::from(colour.r()) | u32::from(colour.g()) << 8 | u32::from(colour.b()) << 16
}

fn point(lparam: LPARAM) -> POINT {
    POINT {
        x: (lparam & 0xFFFF) as i16 as i32,
        y: ((lparam >> 16) & 0xFFFF) as i16 as i32,
    }
}

// The drag as a rectangle inside the window's client area.
fn rect_of(hwnd: HWND, from: POINT, to: POINT) -> RECT {
    let mut client: RECT = RECT::default();
    // SAFETY: a live window and a live out parameter.
    unsafe { GetClientRect(hwnd, &mut client) };
    RECT {
        left: from.x.min(to.x).clamp(0, client.right),
        top: from.y.min(to.y).clamp(0, client.bottom),
        right: from.x.max(to.x).clamp(0, client.right),
        bottom: from.y.max(to.y).clamp(0, client.bottom),
    }
}

fn region_of(rect: RECT) -> Region {
    Region {
        left: rect.left as u32,
        top: rect.top as u32,
        width: (rect.right - rect.left) as u32,
        height: (rect.bottom - rect.top) as u32,
    }
}

fn big_enough(region: Region) -> bool {
    region.width >= SMALLEST_REGION && region.height >= SMALLEST_REGION
}

extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_SETCURSOR => {
            // SAFETY: a system cursor, which needs no freeing.
            unsafe { SetCursor(LoadCursorW(ptr::null_mut(), IDC_CROSS)) };
            1
        }
        WM_LBUTTONDOWN => {
            let at = point(lparam);
            let others = with_picker(|picker| {
                let others = [picker.small.take(), picker.drag.map(|(was, ..)| was)];
                picker.drag = Some((hwnd, at, at));
                others
            });
            // SAFETY: a live window of this thread.
            unsafe { SetCapture(hwnd) };
            for other in others.into_iter().flatten().flatten() {
                redraw(other);
            }
            redraw(hwnd);
            0
        }
        WM_MOUSEMOVE => {
            let moved = with_picker(|picker| match &mut picker.drag {
                Some((on, _, now)) if *on == hwnd => {
                    *now = point(lparam);
                    true
                }
                _ => false,
            });
            if moved == Some(true) {
                redraw(hwnd);
            }
            0
        }
        WM_LBUTTONUP => {
            // SAFETY: a call with no arguments.
            unsafe { ReleaseCapture() };
            let taken = with_picker(|picker| {
                let (on, from, _) = picker.drag.take()?;
                let region = region_of(rect_of(on, from, point(lparam)));
                if !big_enough(region) {
                    picker.drag = Some((on, from, point(lparam)));
                    picker.small = Some(on);
                    return None;
                }
                let index = picker.windows.iter().find(|(w, _)| *w == on)?.1;
                let monitor = &picker.monitors[index];
                picker.picked = Some(Picked {
                    monitor: monitor.id.clone(),
                    refresh_hz: monitor.refresh_hz,
                    region,
                });
                Some(())
            });
            match taken.flatten() {
                // SAFETY: a call with no pointers.
                Some(()) => unsafe { PostQuitMessage(0) },
                None => redraw(hwnd),
            }
            0
        }
        WM_RBUTTONUP => {
            // SAFETY: a call with no pointers.
            unsafe { PostQuitMessage(0) };
            0
        }
        WM_KEYDOWN if wparam == usize::from(VK_ESCAPE) => {
            // SAFETY: a call with no pointers.
            unsafe { PostQuitMessage(0) };
            0
        }
        WM_ACTIVATEAPP if wparam == 0 => {
            // SAFETY: a call with no pointers.
            unsafe { PostQuitMessage(0) };
            0
        }
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            paint(hwnd);
            0
        }
        // SAFETY: the default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn with_picker<R>(f: impl FnOnce(&mut Picker) -> R) -> Option<R> {
    PICKER.with(|picker| picker.borrow_mut().as_mut().map(f))
}

fn redraw(hwnd: HWND) {
    // SAFETY: a live window of this thread; null is the whole client area.
    unsafe { InvalidateRect(hwnd, ptr::null(), 0) };
}

fn paint(hwnd: HWND) {
    let (drag, small) = with_picker(|picker| (picker.drag, picker.small)).unwrap_or((None, None));
    let mut client = RECT::default();
    // SAFETY: as in rect_of.
    unsafe { GetClientRect(hwnd, &mut client) };
    // SAFETY: a live window, after which the paint struct is the one
    // BeginPaint filled; nothing here outlives EndPaint.
    unsafe {
        let mut paint: PAINTSTRUCT = mem::zeroed();
        let screen = BeginPaint(hwnd, &mut paint);
        // Drawn off screen first, so the rectangle never flickers.
        let memory = CreateCompatibleDC(screen);
        let bitmap = CreateCompatibleBitmap(screen, client.right, client.bottom);
        let old_bitmap = SelectObject(memory, bitmap);
        fill(memory, client, colour(WINDOW));
        let dpi = GetDpiForWindow(hwnd).max(96) as i32;
        let scaled = |px: i32| px * dpi / 96;
        let font = font(scaled(TEXT_PX));
        let old_font = (!font.is_null()).then(|| SelectObject(memory, font));
        SetBkMode(memory, TRANSPARENT as i32);
        if let Some((on, from, to)) = drag.filter(|(on, ..)| *on == hwnd) {
            let rect = rect_of(on, from, to);
            let edge = scaled(EDGE);
            fill(
                memory,
                RECT {
                    left: rect.left - edge,
                    top: rect.top - edge,
                    right: rect.right + edge,
                    bottom: rect.bottom + edge,
                },
                colour(AMBER),
            );
            fill(memory, rect, KEY);
            let region = region_of(rect);
            let too_small = small == Some(hwnd);
            let (words, tone) = if too_small {
                (messages::region_too_small(region.width, region.height), BAD)
            } else {
                (format!("{}x{}", region.width, region.height), CHALK)
            };
            let below = rect.bottom + edge + scaled(PAD);
            let at = if below + scaled(TEXT_PX + 2 * PAD) < client.bottom {
                below
            } else {
                rect.top - edge - scaled(PAD + TEXT_PX + 2 * PAD)
            };
            label(
                memory,
                &words,
                (rect.left - edge, at.max(0)),
                tone,
                scaled(PAD),
            );
        }
        let words = messages::REGION_PICK;
        let width = text_width(memory, words) + 2 * scaled(PAD);
        label(
            memory,
            words,
            ((client.right - width) / 2, scaled(3 * PAD)),
            CHALK,
            scaled(PAD),
        );
        BitBlt(
            screen,
            0,
            0,
            client.right,
            client.bottom,
            memory,
            0,
            0,
            SRCCOPY,
        );
        if let Some(old_font) = old_font {
            SelectObject(memory, old_font);
            DeleteObject(font);
        }
        SelectObject(memory, old_bitmap);
        DeleteObject(bitmap);
        DeleteDC(memory);
        EndPaint(hwnd, &paint);
    }
}

fn fill(dc: HDC, rect: RECT, colour: COLORREF) {
    // SAFETY: a live device context; the brush is made and deleted here.
    unsafe {
        let brush = CreateSolidBrush(colour);
        FillRect(dc, &rect, brush);
        DeleteObject(brush);
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

fn text_width(dc: HDC, text: &str) -> i32 {
    let mut text = wide(text);
    let mut rect = RECT::default();
    // SAFETY: a live device context and a buffer of the length passed;
    // DT_CALCRECT only measures.
    unsafe {
        DrawTextW(
            dc,
            text.as_mut_ptr(),
            text.len() as i32,
            &mut rect,
            DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
        )
    };
    rect.right - rect.left
}

// Words on a panel-tone box whose top left corner is `at`.
fn label(dc: HDC, text: &str, at: (i32, i32), tone: Color32, pad: i32) {
    let mut wide_text = wide(text);
    let mut measured = RECT::default();
    // SAFETY: as in text_width.
    unsafe {
        DrawTextW(
            dc,
            wide_text.as_mut_ptr(),
            wide_text.len() as i32,
            &mut measured,
            DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
        )
    };
    let width = measured.right - measured.left;
    let height = measured.bottom - measured.top;
    let left = at.0.max(0);
    let top = at.1;
    fill(
        dc,
        RECT {
            left,
            top,
            right: left + width + 2 * pad,
            bottom: top + height + 2 * pad,
        },
        colour(PANEL),
    );
    let mut rect = RECT {
        left: left + pad,
        top: top + pad,
        right: left + pad + width,
        bottom: top + pad + height,
    };
    // SAFETY: a live device context and a buffer of the length passed.
    unsafe {
        SetTextColor(dc, colour(tone));
        DrawTextW(
            dc,
            wide_text.as_mut_ptr(),
            wide_text.len() as i32,
            &mut rect,
            DT_SINGLELINE | DT_NOPREFIX,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW};

    use super::*;

    fn at(x: i32, y: i32) -> LPARAM {
        ((y as u32) << 16 | (x as u32 & 0xFFFF)) as LPARAM
    }

    // The first of the picker's windows once it is open.
    fn window() -> HWND {
        let class: Vec<u16> = CLASS.encode_utf16().chain([0]).collect();
        let started = Instant::now();
        loop {
            // SAFETY: a zero-terminated class name and no title.
            let hwnd = unsafe { FindWindowW(class.as_ptr(), ptr::null()) };
            if !hwnd.is_null() {
                return hwnd;
            }
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "no picker window"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn post(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) {
        // SAFETY: a window of the picker's thread, which is alive while it
        // waits for an answer.
        unsafe { PostMessageW(hwnd, msg, wparam, lparam) };
    }

    // A drag takes that rectangle of the monitor it was on, in its pixels;
    // one too small is not taken, and Esc then takes nothing. The windows
    // show for a moment on every monitor.
    #[test]
    fn a_drag_takes_that_region() {
        share::make_process_dpi_aware().unwrap();
        let monitors = share::monitors().unwrap();
        if monitors.is_empty() {
            println!("skipped: no monitor is attached to the desktop");
            return;
        }
        let answer = pick(monitors.clone(), || {}).unwrap();
        let hwnd = window();
        post(hwnd, WM_LBUTTONDOWN, 0, at(100, 50));
        post(hwnd, WM_MOUSEMOVE, 0, at(300, 200));
        post(hwnd, WM_LBUTTONUP, 0, at(500, 400));
        let picked = answer
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap()
            .expect("a region");
        assert_eq!(
            picked.region,
            Region {
                left: 100,
                top: 50,
                width: 400,
                height: 350,
            }
        );
        assert!(monitors.iter().any(|monitor| monitor.id == picked.monitor));
        let answer = pick(monitors, || {}).unwrap();
        let hwnd = window();
        post(hwnd, WM_LBUTTONDOWN, 0, at(100, 50));
        post(hwnd, WM_LBUTTONUP, 0, at(200, 400));
        post(hwnd, WM_KEYDOWN, usize::from(VK_ESCAPE), 0);
        let picked = answer
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        assert!(picked.is_none());
    }

    #[test]
    fn no_monitor_takes_nothing() {
        let answer = pick(Vec::new(), || {}).unwrap();
        let picked = answer
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        assert!(picked.is_none());
    }
}
