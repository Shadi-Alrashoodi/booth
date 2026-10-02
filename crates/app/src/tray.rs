// The tray icon: the strip's trace as a mark, which says by its shape what
// state Booth is in, so it still says it over a game in
// exclusive fullscreen, where the control indicator cannot show. A flat
// line when the link is good, a jagged one when it is not, an empty ring
// while connecting or lost, a hollow square while a control request waits,
// a filled square while someone controls this PC, and a slash across it
// while muted or deafened. A click brings the panel forward, and so does
// starting Booth again on the same profile.
//
// It lives on a thread of its own with a hidden window, which Windows tells
// about clicks and about Explorer starting again, and is removed when the
// tray is dropped.

use std::cell::RefCell;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::{mem, ptr};

use eframe::egui::Color32;
use room::view::{Level, LinkState, Strip, View};
use windows_sys::Win32::Foundation::{ERROR_CLASS_ALREADY_EXISTS, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
    NIN_SELECT, NOTIFY_ICON_MESSAGE, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIconFromResourceEx, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyWindow,
    DispatchMessageW, GetMessageW, GetSystemMetrics, HICON, LR_DEFAULTCOLOR, MSG, PostMessageW,
    PostQuitMessage, RegisterClassExW, RegisterWindowMessageW, SM_CXSMICON, WM_APP, WM_CLOSE,
    WM_DESTROY, WNDCLASSEXW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::messages;
use crate::theme::{AMBER, ASH, CHALK, INK, SAGE, WARN};

// Windows' messages about the icon, and the panel's word that the look
// changed.
const CALLBACK: u32 = WM_APP + 1;
const UPDATE: u32 = WM_APP + 2;
// Found by a second copy on the same profile (running.rs), which asks for
// the panel with the message named here.
pub const CLASS: &str = "BoothTray";
const SHOW_PANEL: &str = "BoothShowPanel";
// windows-sys leaves this one out: NIN_SELECT with NINF_KEY, Enter or Space
// on the icon.
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
const ICON_ID: u32 = 1;
// What CreateIconFromResourceEx wants for an icon made from bytes.
const ICON_FORMAT: u32 = 0x0003_0000;
const TIP_UNITS: usize = 127;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mark {
    // The link is good, or there is no room: the app icon.
    #[default]
    Flat,
    Jagged,
    Ring,
    Hollow,
    Filled,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Look {
    pub mark: Mark,
    pub slash: bool,
    // What Windows shows and a screen reader reads for the icon.
    pub tip: String,
}

// From the view of the room this panel is in, or None outside one. Control
// comes first: it is the state the icon is there to show over a game. The
// squares show only while the panel offers control (screens/control.rs).
pub fn look(view: Option<&View>, control_offered: bool) -> Look {
    let Some(view) = view else {
        return Look {
            mark: Mark::Flat,
            slash: false,
            tip: String::from("Booth"),
        };
    };
    let control = &view.share.control;
    let controlled_by = control.controlled_by.as_ref().filter(|_| control_offered);
    let asked_by = control.asked_by.as_ref().filter(|_| control_offered);
    let (mark, tip) = if let Some(party) = controlled_by {
        let said = messages::controlling_this_pc(&party.name);
        (Mark::Filled, format!("Booth: {said}"))
    } else if let Some(asked) = asked_by {
        let said = messages::wants_control(&asked.name);
        (Mark::Hollow, format!("Booth: {said}"))
    } else {
        (link_mark(&view.strip), String::from("Booth"))
    };
    Look {
        mark,
        slash: view.voice.muted || view.voice.deafened,
        tip,
    }
}

// The strip's own reading of the link.
fn link_mark(strip: &Strip) -> Mark {
    match strip.state {
        LinkState::Alone => Mark::Flat,
        LinkState::Connecting | LinkState::Lost | LinkState::Closed => Mark::Ring,
        LinkState::Reconnecting => Mark::Jagged,
        LinkState::Live => {
            let levels = [strip.rtt_level, strip.jitter_level, strip.loss_level];
            if levels.iter().all(|level| *level == Level::Good) {
                Mark::Flat
            } else {
                Mark::Jagged
            }
        }
    }
}

// The icon `size` pixels square, row by row from the top, in RGBA. Drawn on
// a 16 unit grid, the tray's size at 100 percent, and scaled with it, on an
// ink square like the app icon.
pub fn pixels(look: &Look, size: u32) -> Vec<[u8; 4]> {
    let unit = size as f32 / 16.0;
    let mut out = Vec::with_capacity((size * size) as usize);
    for y in 0..size {
        for x in 0..size {
            let at = ((x as f32 + 0.5) / unit, (y as f32 + 0.5) / unit);
            let mut color = mark_at(look.mark, at).unwrap_or(INK);
            if look.slash {
                // An ink edge keeps the slash readable over the filled
                // square, which is as bright as it is.
                let off = to_segment(at, (2.5, 13.5), (13.5, 2.5));
                if off <= 0.9 {
                    color = CHALK;
                } else if off <= 1.9 {
                    color = INK;
                }
            }
            out.push(color.to_array());
        }
    }
    out
}

fn mark_at(mark: Mark, (x, y): (f32, f32)) -> Option<Color32> {
    let within = |low: f32, high: f32| (low..=high).contains(&x) && (low..=high).contains(&y);
    let hit = match mark {
        Mark::Flat => (2.5..=13.5).contains(&x) && (11.0..=12.5).contains(&y),
        Mark::Jagged => {
            const POINTS: [(f32, f32); 5] = [
                (2.5, 12.0),
                (5.0, 6.0),
                (7.5, 12.0),
                (10.0, 6.0),
                (13.5, 12.0),
            ];
            POINTS
                .windows(2)
                .any(|pair| to_segment((x, y), pair[0], pair[1]) <= 0.8)
        }
        Mark::Ring => ((x - 8.0).hypot(y - 8.0) - 5.0).abs() <= 0.8,
        Mark::Hollow => within(3.0, 13.0) && !(x > 4.5 && x < 11.5 && y > 4.5 && y < 11.5),
        Mark::Filled => within(3.0, 13.0),
    };
    let color = match mark {
        Mark::Flat => SAGE,
        Mark::Jagged => WARN,
        Mark::Ring => ASH,
        Mark::Hollow | Mark::Filled => AMBER,
    };
    hit.then_some(color)
}

fn to_segment((x, y): (f32, f32), (ax, ay): (f32, f32), (bx, by): (f32, f32)) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let along = (((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    (x - (ax + along * dx)).hypot(y - (ay + along * dy))
}

pub struct Tray {
    window: isize,
    thread: Option<JoinHandle<()>>,
    wanted: Arc<Mutex<Look>>,
    shown: Option<Look>,
    added: Arc<AtomicBool>,
}

impl Tray {
    // `click` runs on the tray's thread when the icon is clicked or chosen
    // with the keyboard, and must return at once.
    pub fn start(click: impl Fn() + Send + 'static) -> io::Result<Tray> {
        let wanted = Arc::new(Mutex::new(look(None, false)));
        let added = Arc::new(AtomicBool::new(false));
        let (ready, opened) = mpsc::channel();
        let (thread_wanted, thread_added) = (Arc::clone(&wanted), Arc::clone(&added));
        let thread = thread::Builder::new()
            .name(String::from("booth tray"))
            .spawn(move || {
                let here = Here {
                    wanted: thread_wanted,
                    added: thread_added,
                    click: Box::new(click),
                    icon: ptr::null_mut(),
                    hwnd: ptr::null_mut(),
                    taskbar_created: 0,
                    show_panel: 0,
                };
                run(here, &ready);
            })
            .map_err(|err| context("could not start the tray thread", err))?;
        let window = match opened.recv() {
            Ok(Ok(window)) => window,
            Ok(Err(err)) => {
                let _ = thread.join();
                return Err(err);
            }
            Err(_) => {
                let _ = thread.join();
                return Err(io::Error::other(
                    "the tray thread ended before its window was made",
                ));
            }
        };
        Ok(Tray {
            window,
            thread: Some(thread),
            wanted,
            shown: None,
            added,
        })
    }

    // Every pass of the panel, which also runs while it is minimized. Only
    // a change goes to the tray's thread.
    pub fn show(&mut self, look: Look) {
        if self.shown.as_ref() == Some(&look) {
            return;
        }
        *lock(&self.wanted) = look.clone();
        self.shown = Some(look);
        // SAFETY: the window is the tray thread's and lives until Drop
        // closes it; a post to a window that is gone fails harmlessly.
        unsafe { PostMessageW(self.window as HWND, UPDATE, 0, 0) };
    }

    // Whether the icon is on the taskbar. False while Explorer is not
    // running; it goes back when Explorer starts again.
    pub fn on_taskbar(&self) -> bool {
        self.added.load(Ordering::Acquire)
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        // SAFETY: as in show. WM_CLOSE takes the icon off and ends the
        // thread's loop.
        unsafe { PostMessageW(self.window as HWND, WM_CLOSE, 0, 0) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// What the tray's thread keeps, for its window procedure.
struct Here {
    wanted: Arc<Mutex<Look>>,
    added: Arc<AtomicBool>,
    click: Box<dyn Fn() + Send>,
    icon: HICON,
    hwnd: HWND,
    // Sent to every top-level window when Explorer starts, so the icon
    // goes back on a new taskbar.
    taskbar_created: u32,
    show_panel: u32,
}

thread_local! {
    static HERE: RefCell<Option<Here>> = const { RefCell::new(None) };
}

fn run(mut here: Here, ready: &mpsc::Sender<io::Result<isize>>) {
    let hwnd = match open_window() {
        Ok(hwnd) => hwnd,
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };
    here.taskbar_created = register("TaskbarCreated");
    here.show_panel = show_message();
    here.hwnd = hwnd;
    here.add();
    HERE.with(|slot| *slot.borrow_mut() = Some(here));
    let _ = ready.send(Ok(hwnd as isize));
    // SAFETY: MSG is plain data that GetMessageW fills; the loop ends on
    // WM_QUIT (0) or an error (-1).
    unsafe {
        let mut message: MSG = mem::zeroed();
        while GetMessageW(&mut message, ptr::null_mut(), 0, 0) > 0 {
            DispatchMessageW(&message);
        }
    }
    HERE.with(|slot| slot.borrow_mut().take());
}

// 0 if Windows refused, which the window procedure then never answers.
fn register(name: &str) -> u32 {
    let name: Vec<u16> = name.encode_utf16().chain([0]).collect();
    // SAFETY: a zero-terminated name that outlives the call.
    unsafe { RegisterWindowMessageW(name.as_ptr()) }
}

// The same number in every process that asks by this name.
pub fn show_message() -> u32 {
    register(SHOW_PANEL)
}

fn open_window() -> io::Result<HWND> {
    let class: Vec<u16> = CLASS.encode_utf16().chain([0]).collect();
    // SAFETY: a null name asks for this exe's own module handle, which needs
    // no closing.
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
        // The class stays for the life of the process once made.
        if err.raw_os_error() != Some(ERROR_CLASS_ALREADY_EXISTS as i32) {
            return Err(context("could not register the tray window class", err));
        }
    }
    // Top-level rather than message-only, since Explorer's TaskbarCreated
    // goes only to top-level windows. It is never shown.
    // SAFETY: the class was registered above with this module, the name is
    // zero terminated, and every other pointer is null, which
    // CreateWindowExW allows for a top-level window without a title.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class.as_ptr(),
            ptr::null(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null(),
        )
    };
    if hwnd.is_null() {
        return Err(context(
            "could not make the tray window",
            io::Error::last_os_error(),
        ));
    }
    Ok(hwnd)
}

impl Here {
    fn add(&mut self) {
        let old = self.new_icon();
        let data = self.data();
        // SAFETY: `data` carries its own size and lives across both calls.
        let added = unsafe { Shell_NotifyIconW(NIM_ADD, &data) } != 0;
        if added {
            // SAFETY: as above.
            unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) };
        }
        self.added.store(added, Ordering::Release);
        destroy(old);
    }

    fn update(&mut self) {
        let old = self.new_icon();
        self.notify(NIM_MODIFY);
        destroy(old);
    }

    fn remove(&mut self) {
        if self.added.swap(false, Ordering::AcqRel) {
            self.notify(NIM_DELETE);
        }
    }

    fn notify(&self, message: NOTIFY_ICON_MESSAGE) {
        let data = self.data();
        // SAFETY: `data` carries its own size and lives across the call.
        unsafe { Shell_NotifyIconW(message, &data) };
    }

    // The icon for the look wanted now. Returns the one it replaces, to be
    // destroyed once the tray has the new one; a look Windows would not
    // make an icon of keeps the old one.
    fn new_icon(&mut self) -> HICON {
        let look = lock(&self.wanted).clone();
        let icon = make_icon(&look);
        if icon.is_null() {
            return ptr::null_mut();
        }
        mem::replace(&mut self.icon, icon)
    }

    fn data(&self) -> NOTIFYICONDATAW {
        let look = lock(&self.wanted);
        let mut tip = [0u16; 128];
        for (slot, unit) in tip.iter_mut().zip(look.tip.encode_utf16().take(TIP_UNITS)) {
            *slot = unit;
        }
        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: ICON_ID,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
            uCallbackMessage: CALLBACK,
            hIcon: self.icon,
            szTip: tip,
            ..NOTIFYICONDATAW::default()
        };
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        data
    }
}

impl Drop for Here {
    fn drop(&mut self) {
        destroy(self.icon);
    }
}

// An icon this thread made that the tray no longer shows: Windows copies
// the one it is given.
fn destroy(icon: HICON) {
    if !icon.is_null() {
        // SAFETY: made by make_icon on this thread and held by nothing else.
        unsafe { DestroyIcon(icon) };
    }
}

// An icon from pixels: a 32-bit DIB with an alpha channel, laid out the way
// an .ico file holds one image, which is what CreateIconFromResourceEx
// reads. Null if Windows refuses it.
fn make_icon(look: &Look) -> HICON {
    // SAFETY: a plain query.
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.clamp(16, 64) as u32;
    let bytes = icon_bytes(&pixels(look, size), size);
    // SAFETY: `bytes` holds a whole icon image of the size given and lives
    // across the call, which copies it.
    unsafe {
        CreateIconFromResourceEx(
            bytes.as_ptr(),
            bytes.len() as u32,
            1,
            ICON_FORMAT,
            size as i32,
            size as i32,
            LR_DEFAULTCOLOR,
        )
    }
}

// BITMAPINFOHEADER, then the colour rows bottom up in BGRA, then a mask of
// one bit a pixel, which the alpha makes unused but the format still has.
// The header's height counts both.
fn icon_bytes(pixels: &[[u8; 4]], size: u32) -> Vec<u8> {
    const HEADER: u32 = 40;
    let mask_row = size.div_ceil(32) * 4;
    let mut out = Vec::with_capacity((HEADER + size * size * 4 + mask_row * size) as usize);
    out.extend_from_slice(&HEADER.to_le_bytes());
    out.extend_from_slice(&(size as i32).to_le_bytes());
    out.extend_from_slice(&(2 * size as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    // No compression, and the sizes and colour counts it may leave as zero.
    out.extend_from_slice(&[0; 24]);
    for row in pixels.chunks(size as usize).rev() {
        for [r, g, b, a] in row {
            out.extend_from_slice(&[*b, *g, *r, *a]);
        }
    }
    out.resize(out.len() + (mask_row * size) as usize, 0);
    out
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let handled = HERE.with(|slot| {
        // A message sent while a call below is still running (DestroyWindow
        // sends WM_DESTROY) finds the state borrowed, and gets the default.
        let Ok(mut here) = slot.try_borrow_mut() else {
            return false;
        };
        let Some(here) = here.as_mut() else {
            return false;
        };
        match message {
            CALLBACK => {
                // Version 4: what happened is in the low word.
                let what = u32::from(lparam as u16);
                if what == NIN_SELECT || what == NIN_KEYSELECT {
                    (here.click)();
                }
                true
            }
            UPDATE => {
                here.update();
                true
            }
            WM_CLOSE => {
                here.remove();
                false
            }
            other if other == here.taskbar_created && other != 0 => {
                here.add();
                true
            }
            other if other == here.show_panel && other != 0 => {
                (here.click)();
                true
            }
            _ => false,
        }
    });
    if handled {
        return 0;
    }
    match message {
        WM_CLOSE => {
            // SAFETY: this thread's own window.
            unsafe { DestroyWindow(hwnd) };
            0
        }
        WM_DESTROY => {
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            0
        }
        // SAFETY: the arguments are the ones Windows called with.
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn lock(look: &Mutex<Look>) -> MutexGuard<'_, Look> {
    look.lock().unwrap_or_else(PoisonError::into_inner)
}

fn context(what: &str, err: io::Error) -> io::Error {
    io::Error::new(err.kind(), format!("{what}: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use room::view::{ControlRequest, ControlView, Party, ShareView, Voice};

    // The control switch (screens/control.rs): on once the app has an
    // injector.
    const ON: bool = true;
    const OFF: bool = false;

    fn room(state: LinkState, control: ControlView) -> View {
        View {
            role: room::view::Role::Client,
            room_name: String::new(),
            strip: Strip {
                state,
                ..Strip::default()
            },
            people: Vec::new(),
            invite: None,
            numbers: Default::default(),
            chat: Default::default(),
            notice: None,
            reply: None,
            paste: None,
            address_changed: None,
            list_problem: None,
            voice: Voice::default(),
            share: ShareView {
                control,
                ..ShareView::default()
            },
        }
    }

    fn mark(view: &View) -> Mark {
        look(Some(view), ON).mark
    }

    #[test]
    fn marks_by_state() {
        assert_eq!(
            look(None, ON),
            Look {
                mark: Mark::Flat,
                slash: false,
                tip: String::from("Booth")
            }
        );
        let idle = ControlView::default();
        assert_eq!(mark(&room(LinkState::Alone, idle.clone())), Mark::Flat);
        assert_eq!(mark(&room(LinkState::Live, idle.clone())), Mark::Flat);
        assert_eq!(
            mark(&room(LinkState::Reconnecting, idle.clone())),
            Mark::Jagged
        );
        for state in [LinkState::Connecting, LinkState::Lost, LinkState::Closed] {
            assert_eq!(mark(&room(state, idle.clone())), Mark::Ring, "{state:?}");
        }
        let mut slow = room(LinkState::Live, idle.clone());
        slow.strip.loss_level = Level::Warn;
        assert_eq!(mark(&slow), Mark::Jagged);

        // "control requested": the hollow square, whatever the link.
        let asked = ControlView {
            asked_by: Some(ControlRequest {
                number: 3,
                key: [9; 32],
                name: String::from("Tom"),
            }),
            ..ControlView::default()
        };
        let waiting = look(Some(&room(LinkState::Reconnecting, asked)), ON);
        assert_eq!(waiting.mark, Mark::Hollow);
        assert_eq!(
            waiting.tip,
            "Booth: \u{2068}Tom\u{2069} wants to control your screen."
        );

        // "being controlled": the filled square.
        let controlled = ControlView {
            controller: Some([9; 32]),
            controlled_by: Some(Party {
                key: [9; 32],
                name: String::from("Tom"),
            }),
            ..ControlView::default()
        };
        let mut view = room(LinkState::Live, controlled);
        let shown = look(Some(&view), ON);
        assert_eq!(shown.mark, Mark::Filled);
        assert_eq!(
            shown.tip,
            "Booth: \u{2068}Tom\u{2069} is controlling this PC."
        );

        // "muted, deafened": a slash across whatever else it shows.
        assert!(!shown.slash);
        view.voice.muted = true;
        assert_eq!(look(Some(&view), ON).mark, Mark::Filled);
        assert!(look(Some(&view), ON).slash);
        view.voice = Voice {
            deafened: true,
            ..Voice::default()
        };
        assert!(look(Some(&view), ON).slash);
    }

    // With the switch off neither square shows, even where the view says
    // someone asks or controls; the icon says the link, and the slash stays.
    #[test]
    fn no_control_marks_with_the_switch_off() {
        let asked = ControlView {
            asked_by: Some(ControlRequest {
                number: 3,
                key: [9; 32],
                name: String::from("Tom"),
            }),
            ..ControlView::default()
        };
        let controlled = ControlView {
            controller: Some([9; 32]),
            controlled_by: Some(Party {
                key: [9; 32],
                name: String::from("Tom"),
            }),
            ..ControlView::default()
        };
        for control in [asked, controlled] {
            let quiet = look(Some(&room(LinkState::Reconnecting, control.clone())), OFF);
            assert_eq!(quiet.mark, Mark::Jagged, "{control:?}");
            assert_eq!(quiet.tip, "Booth", "{control:?}");
            let mut view = room(LinkState::Live, control.clone());
            view.voice.muted = true;
            let muted = look(Some(&view), OFF);
            assert_eq!((muted.mark, muted.slash), (Mark::Flat, true), "{control:?}");
            assert_ne!(look(Some(&view), ON).mark, Mark::Flat, "{control:?}");
        }
    }

    fn at(image: &[[u8; 4]], size: u32, x: u32, y: u32) -> Color32 {
        let [r, g, b, a] = image[(y * size + x) as usize];
        Color32::from_rgba_premultiplied(r, g, b, a)
    }

    fn drawn(mark: Mark, slash: bool, size: u32) -> Vec<[u8; 4]> {
        let look = Look {
            mark,
            slash,
            tip: String::new(),
        };
        pixels(&look, size)
    }

    // Each shape where it belongs, at 100 and 200 percent.
    #[test]
    fn each_mark_is_drawn_where_its_shape_is() {
        for size in [16, 32] {
            let k = size / 16;
            let flat = drawn(Mark::Flat, false, size);
            assert_eq!(flat.len(), (size * size) as usize);
            assert_eq!(at(&flat, size, 8 * k, 11 * k), SAGE);
            assert_eq!(at(&flat, size, 8 * k, 4 * k), INK);
            let filled = drawn(Mark::Filled, false, size);
            assert_eq!(at(&filled, size, 8 * k, 8 * k), AMBER);
            assert_eq!(at(&filled, size, k, k), INK);
            let hollow = drawn(Mark::Hollow, false, size);
            assert_eq!(at(&hollow, size, 8 * k, 8 * k), INK);
            assert_eq!(at(&hollow, size, 3 * k, 8 * k), AMBER);
            let ring = drawn(Mark::Ring, false, size);
            assert_eq!(at(&ring, size, 8 * k, 8 * k), INK);
            assert_eq!(at(&ring, size, 8 * k, 3 * k), ASH);
            let jagged = drawn(Mark::Jagged, false, size);
            assert_eq!(at(&jagged, size, 5 * k, 6 * k), WARN);
            assert_eq!(at(&jagged, size, 8 * k, 3 * k), INK);
            // The slash crosses the middle, with ink on either side of it
            // over the filled square.
            let slashed = drawn(Mark::Filled, true, size);
            assert_eq!(at(&slashed, size, 8 * k, 8 * k), CHALK);
            assert_eq!(at(&slashed, size, 10 * k, 10 * k), AMBER);
        }
    }

    // The bytes CreateIconFromResourceEx reads: the header, the colour rows
    // bottom up in BGRA, then the mask.
    #[test]
    fn icon_bytes_layout() {
        let size = 16;
        let image = drawn(Mark::Filled, false, size);
        let bytes = icon_bytes(&image, size);
        assert_eq!(bytes.len(), 40 + 16 * 16 * 4 + 16 * 4);
        assert_eq!(&bytes[..4], &40u32.to_le_bytes());
        assert_eq!(&bytes[8..12], &32i32.to_le_bytes());
        assert_eq!(&bytes[14..16], &32u16.to_le_bytes());
        // The first colour row is the bottom one: ink, opaque.
        let [r, g, b, a] = INK.to_array();
        assert_eq!(&bytes[40..44], &[b, g, r, a]);
    }

    // The one test that touches the real tray: the icon goes on when the
    // tray starts, takes a new look, shows the panel when a second copy
    // asks, and comes off when it is dropped.
    #[test]
    fn icon_on_and_off_the_taskbar() {
        let (clicked, click) = mpsc::channel();
        let mut tray = Tray::start(move || {
            let _ = clicked.send(());
        })
        .expect("the tray starts");
        assert!(tray.on_taskbar(), "Explorer took the icon");
        let asked = std::time::Instant::now();
        assert!(crate::running::bring_forward(std::process::id()));
        click
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the panel was asked for");
        println!(
            "a second copy's ask reached the panel in {:?}",
            asked.elapsed()
        );
        tray.show(Look {
            mark: Mark::Hollow,
            slash: true,
            tip: String::from("Booth: a test of the tray icon"),
        });
        let added = Arc::clone(&tray.added);
        drop(tray);
        assert!(!added.load(Ordering::Acquire), "the icon came off");
    }
}
