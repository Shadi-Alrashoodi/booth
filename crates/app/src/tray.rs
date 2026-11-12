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
use std::time::Duration;
use std::{mem, ptr};

use eframe::egui::Color32;
use room::view::{Level, LinkState, Strip, View};
use windows_sys::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, ERROR_NOT_ENOUGH_QUOTA, HWND, LPARAM, LRESULT, WPARAM,
};
use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
    NIN_SELECT, NOTIFY_ICON_MESSAGE, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIconFromResourceEx, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyWindow,
    DispatchMessageW, GetMessageW, GetSystemMetrics, HICON, KillTimer, LR_DEFAULTCOLOR, MSG,
    PostMessageW, PostQuitMessage, RegisterClassExW, RegisterWindowMessageW, SM_CXSMICON, SetTimer,
    WM_APP, WM_CLOSE, WM_COPYDATA, WM_DESTROY, WM_TIMER, WNDCLASSEXW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::theme::{AMBER, ASH, CHALK, INK, SAGE, WARN};
use crate::{links, messages};

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
// The timer that tries a refused icon call again until it works.
const RETRY: usize = 1;
const RETRY_MS: u32 = 2000;

// Shell_NotifyIconW, or a stand-in in tests. True if it worked.
type Shell = Box<dyn Fn(NOTIFY_ICON_MESSAGE, &NOTIFYICONDATAW) -> bool + Send>;

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
    let mut said = Vec::new();
    let mark = if let Some(party) = controlled_by {
        said.push(messages::controlling_this_pc(&party.name));
        Mark::Filled
    } else if let Some(asked) = asked_by {
        said.push(messages::wants_control(&asked.name));
        Mark::Hollow
    } else {
        link_mark(&view.strip)
    };
    if view.share.own_sound.on {
        said.push(messages::YOUR_SHARE_HAS_PC_SOUND.to_owned());
    }
    let tip = if said.is_empty() {
        String::from("Booth")
    } else {
        format!("Booth: {}", said.join(" "))
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
    pub fn start(
        click: impl Fn() + Send + 'static,
        link: impl Fn(&[u8]) -> LRESULT + Send + 'static,
    ) -> io::Result<Tray> {
        Tray::with_shell(click, link, Box::new(shell))
    }

    fn with_shell(
        click: impl Fn() + Send + 'static,
        link: impl Fn(&[u8]) -> LRESULT + Send + 'static,
        shell: Shell,
    ) -> io::Result<Tray> {
        let wanted = Arc::new(Mutex::new(look(None, false)));
        let added = Arc::new(AtomicBool::new(false));
        let (ready, opened) = mpsc::channel();
        let (thread_wanted, thread_added) = (Arc::clone(&wanted), Arc::clone(&added));
        let thread = thread::Builder::new()
            .name(String::from("booth tray"))
            .spawn(move || {
                LINK.with(|slot| *slot.borrow_mut() = Some(Box::new(link)));
                let here = Here {
                    wanted: thread_wanted,
                    added: thread_added,
                    click: Box::new(click),
                    shell,
                    icon: ptr::null_mut(),
                    hwnd: ptr::null_mut(),
                    taskbar_created: 0,
                    show_panel: 0,
                };
                run(here, &ready);
                LINK.with(|slot| slot.borrow_mut().take());
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

    // Whether Windows took the icon's last add or change. Nothing says when
    // Explorer exits, so it stays true then.
    pub fn on_taskbar(&self) -> bool {
        self.added.load(Ordering::Acquire)
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        // SAFETY: as in show. WM_CLOSE takes the icon off and ends the
        // thread's loop.
        while unsafe { PostMessageW(self.window as HWND, WM_CLOSE, 0, 0) } == 0 {
            // A full queue takes it once the thread catches up; any other
            // refusal means the window and its thread are gone.
            let error = io::Error::last_os_error().raw_os_error();
            if error != Some(ERROR_NOT_ENOUGH_QUOTA as i32) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
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
    shell: Shell,
    icon: HICON,
    hwnd: HWND,
    // Sent to every top-level window when Explorer starts, so the icon
    // goes back on a new taskbar.
    taskbar_created: u32,
    show_panel: u32,
}

thread_local! {
    static HERE: RefCell<Option<Here>> = const { RefCell::new(None) };
    static LINK: RefCell<Option<Link>> = const { RefCell::new(None) };
}

type Link = Box<dyn Fn(&[u8]) -> LRESULT + Send>;

unsafe fn link_bytes<'a>(lparam: LPARAM) -> Option<&'a [u8]> {
    if lparam == 0 {
        return None;
    }
    // SAFETY: WM_COPYDATA gives a struct that stays alive through this call.
    let data = unsafe { &*(lparam as *const COPYDATASTRUCT) };
    if data.dwData != links::MESSAGE_TAG
        || data.cbData == 0
        || data.cbData as usize > links::LINK_BYTES
        || data.lpData.is_null()
    {
        return None;
    }
    // The sender's memory is valid only until this call returns.
    // SAFETY: Windows made cbData bytes readable for WM_COPYDATA's handler.
    let bytes =
        unsafe { std::slice::from_raw_parts(data.lpData.cast::<u8>(), data.cbData as usize) };
    Some(bytes)
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
        let added = (self.shell)(NIM_ADD, &data);
        if added {
            (self.shell)(NIM_SETVERSION, &data);
        }
        self.added.store(added, Ordering::Release);
        self.retry(!added);
        destroy(old);
    }

    fn update(&mut self) {
        if !self.added.load(Ordering::Acquire) {
            // The icon can still be on after a refused call, and Windows
            // then refuses the add.
            self.notify(NIM_DELETE);
            self.add();
            return;
        }
        let old = self.new_icon();
        let updated = self.notify(NIM_MODIFY);
        // Tried again as a new add, which also puts back an icon lost to an
        // Explorer restart this window did not hear about.
        self.added.store(updated, Ordering::Release);
        self.retry(!updated);
        destroy(old);
    }

    fn remove(&mut self) {
        // Also after a refused add, which Windows may have carried out.
        if self.notify(NIM_DELETE) {
            self.added.store(false, Ordering::Release);
        }
    }

    fn notify(&self, message: NOTIFY_ICON_MESSAGE) -> bool {
        (self.shell)(message, &self.data())
    }

    fn retry(&self, again: bool) {
        // SAFETY: this thread's own window. Setting the timer again restarts
        // it, and killing one that is not set does nothing.
        unsafe {
            if again {
                SetTimer(self.hwnd, RETRY, RETRY_MS, None);
            } else {
                KillTimer(self.hwnd, RETRY);
            }
        }
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

fn shell(message: NOTIFY_ICON_MESSAGE, data: &NOTIFYICONDATAW) -> bool {
    // SAFETY: `data` carries its own size and lives across the call.
    unsafe { Shell_NotifyIconW(message, data) != 0 }
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
    if message == WM_COPYDATA {
        // HERE can still be borrowed by a click that sent this message.
        // SAFETY: the message's pointers live until this window procedure returns.
        return unsafe { link_bytes(lparam) }.map_or(0, |bytes| {
            LINK.with(|slot| slot.borrow().as_ref().map_or(0, |link| link(bytes)))
        });
    }
    let handled = HERE.with(|slot| {
        // A message sent while a call below is still running finds the
        // state borrowed, and gets the default.
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
            WM_TIMER if wparam == RETRY => {
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
    use std::time::Instant;
    use zeroize::Zeroizing;

    type Inbox = Arc<Mutex<Option<Zeroizing<Vec<u8>>>>>;
    static TRAYS: Mutex<()> = Mutex::new(());

    fn receiving() -> (Inbox, Arc<AtomicBool>, Arc<AtomicBool>) {
        let inbox = Arc::new(Mutex::new(None));
        let arrived = Arc::new(AtomicBool::new(false));
        let in_room = Arc::new(AtomicBool::new(false));
        let receive = crate::app::link_receiver(
            Arc::clone(&inbox),
            Arc::clone(&arrived),
            Arc::clone(&in_room),
            Arc::new(|| {}),
        );
        LINK.with(|slot| *slot.borrow_mut() = Some(Box::new(receive)));
        (inbox, arrived, in_room)
    }

    fn data(bytes: &mut [u8]) -> COPYDATASTRUCT {
        COPYDATASTRUCT {
            dwData: links::MESSAGE_TAG,
            cbData: bytes.len() as u32,
            lpData: bytes.as_mut_ptr().cast(),
        }
    }

    fn deliver(lparam: LPARAM) -> LRESULT {
        // SAFETY: each test keeps its struct and any bytes alive through the call.
        unsafe { window_proc(ptr::null_mut(), WM_COPYDATA, 0, lparam) }
    }

    #[test]
    fn link_message_rejects_bad_structs() {
        let (inbox, arrived, _) = receiving();
        assert_eq!(deliver(0), 0);
        let mut bytes = vec![b'x'; 2049];
        for invalid in [
            COPYDATASTRUCT {
                dwData: 0,
                ..data(&mut bytes[..1])
            },
            COPYDATASTRUCT {
                cbData: 0,
                ..data(&mut bytes)
            },
            data(&mut bytes),
            COPYDATASTRUCT {
                lpData: ptr::null_mut(),
                ..data(&mut bytes[..1])
            },
        ] {
            assert_eq!(deliver(&raw const invalid as isize), 0);
            assert!(inbox.lock().unwrap().is_none());
            assert!(!arrived.load(Ordering::Acquire));
        }
        LINK.with(|slot| slot.borrow_mut().take());
    }

    #[test]
    fn link_message_copies_and_keeps_the_first() {
        let (inbox, arrived, _) = receiving();
        let mut bytes = vec![b'x'; 2048];
        let data = data(&mut bytes);
        assert_eq!(deliver(&raw const data as isize), 1);
        bytes.fill(b'y');
        assert_eq!(deliver(&raw const data as isize), 1);
        assert!(arrived.load(Ordering::Acquire));
        assert_eq!(
            inbox
                .lock()
                .unwrap()
                .as_deref()
                .map(|bytes| bytes.as_slice()),
            Some(vec![b'x'; 2048].as_slice())
        );
        LINK.with(|slot| slot.borrow_mut().take());
    }

    #[test]
    fn link_message_in_a_room_keeps_nothing() {
        let (inbox, arrived, in_room) = receiving();
        in_room.store(true, Ordering::Release);
        let mut bytes = *b"x";
        let data = data(&mut bytes);
        assert_eq!(deliver(&raw const data as isize), 2);
        assert!(inbox.lock().unwrap().is_none());
        assert!(!arrived.load(Ordering::Acquire));
        LINK.with(|slot| slot.borrow_mut().take());
    }

    #[test]
    fn link_message_during_a_tray_click() {
        let _tray = TRAYS.lock().unwrap_or_else(PoisonError::into_inner);
        use std::sync::atomic::AtomicIsize;
        use std::time::Duration;
        use windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW;
        let inbox: Inbox = Arc::default();
        let receiver = crate::app::link_receiver(
            Arc::clone(&inbox),
            Arc::default(),
            Arc::default(),
            Arc::new(|| {}),
        );
        let window = Arc::new(AtomicIsize::new(0));
        let target = Arc::clone(&window);
        let (sent, answer) = mpsc::channel();
        let tray = Tray::start(
            move || {
                let mut bytes = *b"x";
                let data = data(&mut bytes);
                // SAFETY: the click runs on this tray's thread, with HERE borrowed.
                let taken = unsafe {
                    SendMessageW(
                        target.load(Ordering::Acquire) as HWND,
                        WM_COPYDATA,
                        0,
                        &raw const data as isize,
                    )
                };
                sent.send(taken).unwrap();
            },
            receiver,
        )
        .unwrap();
        window.store(tray.window, Ordering::Release);
        // SAFETY: the tray stays alive until its click answered.
        assert_ne!(
            unsafe { PostMessageW(tray.window as HWND, show_message(), 0, 0) },
            0
        );
        assert_eq!(answer.recv_timeout(Duration::from_secs(2)).unwrap(), 1);
        assert_eq!(
            inbox
                .lock()
                .unwrap()
                .as_deref()
                .map(|bytes| bytes.as_slice()),
            Some(b"x".as_slice())
        );
        drop(tray);
    }

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

    #[test]
    fn tip_names_pc_sound() {
        let mut view = room(LinkState::Live, ControlView::default());
        assert_eq!(look(Some(&view), ON).tip, "Booth");
        view.share.own = room::view::OwnShare::Sharing { number: 1, fps: 60 };
        view.share.running = Some(room::view::RunningShare {
            software: false,
            paused: None,
            held: false,
        });
        assert_eq!(look(Some(&view), ON).tip, "Booth");
        view.share.own_sound.on = true;
        let sound = "Booth: Your share has PC sound.";
        let shown = look(Some(&view), ON);
        assert_eq!((shown.mark, shown.slash), (Mark::Flat, false));
        assert_eq!(shown.tip, sound);
        view.share.running.as_mut().unwrap().held = true;
        assert_eq!(look(Some(&view), ON).tip, sound);
        view.share.running.as_mut().unwrap().paused = Some(room::view::Paused::SecureDesktop);
        view.share.own_sound.lost = Some(String::from("lost"));
        assert_eq!(look(Some(&view), ON).tip, sound);

        // Control first, and the longest name still leaves room for both:
        // longest in the UTF-16 units the tip is cut by, within the 32
        // characters and 64 bytes a name keeps.
        let name = format!("{}{}", "\u{20000}".repeat(11), "W".repeat(20));
        view.share.control.asked_by = Some(ControlRequest {
            number: 3,
            key: [9; 32],
            name: name.clone(),
        });
        let both = look(Some(&view), ON);
        let asked = format!("\u{2068}{name}\u{2069} wants to control your screen.");
        assert_eq!(both.mark, Mark::Hollow);
        assert_eq!(both.tip, format!("Booth: {asked} Your share has PC sound."));
        assert!(both.tip.encode_utf16().count() <= TIP_UNITS);
        assert_eq!(look(Some(&view), OFF).tip, sound);

        view.share.control = ControlView {
            controller: Some([9; 32]),
            controlled_by: Some(Party {
                key: [9; 32],
                name: String::from("Tom"),
            }),
            ..ControlView::default()
        };
        let both = look(Some(&view), ON);
        assert_eq!(both.mark, Mark::Filled);
        assert_eq!(
            both.tip,
            "Booth: \u{2068}Tom\u{2069} is controlling this PC. Your share has PC sound."
        );
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
            assert_eq!(at(&slashed, size, 9 * k, 8 * k), INK);
            assert_eq!(at(&slashed, size, 5 * k, 8 * k), INK);
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
        // The flat line, which is not the same upside down, is on row 11
        // from the top: row 4 from the bottom.
        let flat = icon_bytes(&drawn(Mark::Flat, false, size), size);
        let line = 40 + 4 * 16 * 4 + 8 * 4;
        let [r, g, b, a] = SAGE.to_array();
        assert_eq!(&flat[line..line + 4], &[b, g, r, a]);
    }

    // On the real taskbar: the icon goes on when the tray starts, takes a
    // new look, shows the panel when a second copy asks, and comes off when
    // it is dropped.
    #[test]
    fn icon_on_and_off_the_taskbar() {
        let _tray = TRAYS.lock().unwrap_or_else(PoisonError::into_inner);
        let (clicked, click) = mpsc::channel();
        let (real, calls) = recorded(shell);
        let mut tray = Tray::with_shell(
            move || {
                let _ = clicked.send(());
            },
            |_| 0,
            real,
        )
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
        let tip = String::from("Booth: a test of the tray icon");
        tray.show(Look {
            mark: Mark::Hollow,
            slash: true,
            tip: tip.clone(),
        });
        let added = Arc::clone(&tray.added);
        drop(tray);
        assert!(!added.load(Ordering::Acquire), "the icon came off");
        let calls = calls.lock().unwrap();
        let modified = (NIM_MODIFY, tip.clone(), true);
        assert!(calls.contains(&modified), "{calls:?}");
        assert_eq!(calls.last(), Some(&(NIM_DELETE, tip, true)));
    }

    type Calls = Arc<Mutex<Vec<(NOTIFY_ICON_MESSAGE, String, bool)>>>;

    // A shell that answers as `answer` does and keeps each call: what was
    // asked, the tip and the answer.
    fn recorded(
        answer: impl Fn(NOTIFY_ICON_MESSAGE, &NOTIFYICONDATAW) -> bool + Send + 'static,
    ) -> (Shell, Calls) {
        let calls = Calls::default();
        let kept = Arc::clone(&calls);
        let shell: Shell = Box::new(move |message, data: &NOTIFYICONDATAW| {
            let answered = answer(message, data);
            let tip = String::from_utf16_lossy(&data.szTip);
            let call = (message, tip.trim_end_matches('\0').to_owned(), answered);
            kept.lock().unwrap().push(call);
            answered
        });
        (shell, calls)
    }

    // Refuses the first call that asks for `refused` and takes every other.
    fn refusing_once(
        refused: NOTIFY_ICON_MESSAGE,
    ) -> impl Fn(NOTIFY_ICON_MESSAGE, &NOTIFYICONDATAW) -> bool + Send + 'static {
        let refused_once = AtomicBool::new(false);
        move |message, _| message != refused || refused_once.swap(true, Ordering::AcqRel)
    }

    fn within_retries(done: impl Fn() -> bool) -> bool {
        let until = Instant::now() + 3 * Duration::from_millis(RETRY_MS.into());
        while !done() {
            if Instant::now() > until {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
        true
    }

    #[test]
    fn retries_refused_add() {
        let _tray = TRAYS.lock().unwrap_or_else(PoisonError::into_inner);
        let (shell, calls) = recorded(refusing_once(NIM_ADD));
        let tray = Tray::with_shell(|| {}, |_| 0, shell).unwrap();
        assert!(!tray.on_taskbar());
        let added = within_retries(|| tray.on_taskbar());
        assert!(added, "the add was not tried again");
        // Once it worked, the timer is gone.
        thread::sleep(Duration::from_millis(RETRY_MS.into()) * 3 / 2);
        drop(tray);
        let call = |message, answer| (message, String::from("Booth"), answer);
        assert_eq!(
            *calls.lock().unwrap(),
            [
                call(NIM_ADD, false),
                call(NIM_DELETE, true),
                call(NIM_ADD, true),
                call(NIM_SETVERSION, true),
                call(NIM_DELETE, true),
            ]
        );
    }

    #[test]
    fn retries_refused_modify() {
        let _tray = TRAYS.lock().unwrap_or_else(PoisonError::into_inner);
        let (shell, calls) = recorded(refusing_once(NIM_MODIFY));
        let mut tray = Tray::with_shell(|| {}, |_| 0, shell).unwrap();
        let tip = String::from("Booth: a new look");
        tray.show(Look {
            mark: Mark::Flat,
            slash: true,
            tip: tip.clone(),
        });
        let readded = (NIM_ADD, tip.clone(), true);
        let tried = within_retries(|| calls.lock().unwrap().contains(&readded));
        assert!(tried, "the look was not tried again");
        // Once it worked, the timer is gone.
        thread::sleep(Duration::from_millis(RETRY_MS.into()) * 3 / 2);
        drop(tray);
        let booth = String::from("Booth");
        let call = |message, answer| (message, tip.clone(), answer);
        assert_eq!(
            *calls.lock().unwrap(),
            [
                (NIM_ADD, booth.clone(), true),
                (NIM_SETVERSION, booth, true),
                call(NIM_MODIFY, false),
                call(NIM_DELETE, true),
                readded,
                call(NIM_SETVERSION, true),
                call(NIM_DELETE, true),
            ]
        );
    }

    // Explorer started again and the tray never heard: the icon is gone, and
    // only a new add puts it back.
    #[test]
    fn vanished_icon_comes_back() {
        let _tray = TRAYS.lock().unwrap_or_else(PoisonError::into_inner);
        let on = Arc::new(AtomicBool::new(false));
        let explorer = Arc::clone(&on);
        let (shell, calls) = recorded(move |message, _| match message {
            NIM_ADD => !explorer.swap(true, Ordering::AcqRel),
            NIM_DELETE => explorer.swap(false, Ordering::AcqRel),
            _ => explorer.load(Ordering::Acquire),
        });
        let mut tray = Tray::with_shell(|| {}, |_| 0, shell).unwrap();
        on.store(false, Ordering::Release);
        let tip = String::from("Booth: after a restart");
        tray.show(Look {
            mark: Mark::Ring,
            slash: false,
            tip: tip.clone(),
        });
        let readded = (NIM_ADD, tip, true);
        let back = within_retries(|| calls.lock().unwrap().contains(&readded));
        drop(tray);
        assert!(back, "the icon stayed off");
    }

    // A close refused by a full queue is posted again until the thread takes
    // it, so the drop does not wait for good.
    #[test]
    fn drop_with_full_queue() {
        use windows_sys::Win32::UI::WindowsAndMessaging::WM_NULL;
        let _tray = TRAYS.lock().unwrap_or_else(PoisonError::into_inner);
        let (inside, entered) = mpsc::channel();
        let (release, held) = mpsc::channel::<()>();
        let tray = Tray::with_shell(
            move || {
                let _ = inside.send(());
                let _ = held.recv();
            },
            |_| 0,
            recorded(|_, _| true).0,
        )
        .unwrap();
        let window = tray.window as HWND;
        // SAFETY: the tray's window, which lives until the drop below.
        assert_ne!(unsafe { PostMessageW(window, show_message(), 0, 0) }, 0);
        let held_in_click = entered.recv_timeout(Duration::from_secs(2)).is_ok();
        // SAFETY: as above; a full queue refuses the post.
        let full = (0..100_000).any(|_| unsafe { PostMessageW(window, WM_NULL, 0, 0) } == 0);
        let error = io::Error::last_os_error().raw_os_error();
        let (dropped, done) = mpsc::channel();
        thread::spawn(move || {
            drop(tray);
            let _ = dropped.send(());
        });
        // Long enough for the drop's first post to find the queue full.
        thread::sleep(Duration::from_millis(200));
        release.send(()).unwrap();
        let ended = done.recv_timeout(Duration::from_secs(5)).is_ok();
        // Checked only now: a tray dropped while its click is held never ends.
        assert!(held_in_click);
        assert!(full);
        assert_eq!(error, Some(ERROR_NOT_ENOUGH_QUOTA as i32));
        assert!(ended, "the drop never ended");
    }

    #[test]
    fn drop_after_window_closed() {
        use windows_sys::Win32::UI::WindowsAndMessaging::IsWindow;
        let _tray = TRAYS.lock().unwrap_or_else(PoisonError::into_inner);
        let tray = Tray::with_shell(|| {}, |_| 0, recorded(|_, _| true).0).unwrap();
        let window = tray.window as HWND;
        // SAFETY: the tray's window; a post to it fails once it is gone.
        assert_ne!(unsafe { PostMessageW(window, WM_CLOSE, 0, 0) }, 0);
        // SAFETY: a plain query.
        let closed = within_retries(|| unsafe { IsWindow(window) } == 0);
        assert!(closed, "the window stayed");
        let (dropped, done) = mpsc::channel();
        thread::spawn(move || {
            drop(tray);
            let _ = dropped.send(());
        });
        let ended = done.recv_timeout(Duration::from_secs(5)).is_ok();
        assert!(ended, "the drop never ended");
    }

    #[test]
    fn drop_deletes_refused_add() {
        let _tray = TRAYS.lock().unwrap_or_else(PoisonError::into_inner);
        let (shell, calls) = recorded(refusing_once(NIM_ADD));
        drop(Tray::with_shell(|| {}, |_| 0, shell).unwrap());
        let call = |message, answer| (message, String::from("Booth"), answer);
        assert_eq!(
            *calls.lock().unwrap(),
            [call(NIM_ADD, false), call(NIM_DELETE, true)]
        );
    }

    #[test]
    fn refused_delete_keeps_icon() {
        let _tray = TRAYS.lock().unwrap_or_else(PoisonError::into_inner);
        let shell = Box::new(refusing_once(NIM_DELETE));
        let tray = Tray::with_shell(|| {}, |_| 0, shell).unwrap();
        let added = Arc::clone(&tray.added);
        drop(tray);
        assert!(added.load(Ordering::Acquire), "the icon was taken as off");
    }
}
