use std::cell::RefCell;
use std::ffi::c_void;
use std::io;
#[cfg(feature = "store")]
use std::path::Path;
use std::ptr;
use std::rc::Rc;
use std::sync::{Mutex, PoisonError};

use eframe::egui::Color32;
use input::{Key, Modifiers, PanelKeys};
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, HWND, SYSTEMTIME};
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DwmSetWindowAttribute,
};
use windows_sys::Win32::Security::{
    CheckTokenMembership, CreateWellKnownSid, GetTokenInformation, SECURITY_MAX_SID_SIZE,
    TOKEN_ELEVATION_TYPE, TOKEN_QUERY, TokenElevationType, TokenElevationTypeDefault,
    TokenElevationTypeFull, WinBuiltinAdministratorsSid,
};
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleHandleW, LOAD_LIBRARY_SEARCH_SYSTEM32, SetDefaultDllDirectories,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemTime, GetTickCount};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcessToken,
};
use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows_sys::Win32::System::WindowsProgramming::GetUserNameW;
use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyState, VK_CONTROL, VK_LBUTTON, VK_LWIN, VK_MBUTTON, VK_MENU,
    VK_PROCESSKEY, VK_RBUTTON, VK_RMENU, VK_RWIN, VK_SHIFT, VK_XBUTTON1, VK_XBUTTON2,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, DestroyIcon, DispatchMessageW, HWND_TOP, ICON_BIG, ICON_SMALL,
    IMAGE_ICON, IsIconic, LR_DEFAULTCOLOR, LoadImageW, MB_ICONERROR, MB_OK, MB_TOPMOST, MSG,
    MessageBoxW, PM_QS_POSTMESSAGE, PM_REMOVE, PeekMessageW, SM_CXICON, SM_CXSMICON,
    SPI_GETCLIENTAREAANIMATION, SW_RESTORE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SendMessageW,
    SetForegroundWindow, SetWindowPos, ShowWindow, SystemParametersInfoW, WM_KEYDOWN, WM_KEYUP,
    WM_SETICON, WM_SYSKEYDOWN, WM_SYSKEYUP,
};
use windows_sys::core::BOOL;

use crate::theme;

// UNLEN is 256 characters, plus the terminating zero.
const NAME_CHARS: usize = 257;

pub fn user_name() -> Option<String> {
    let mut buffer = [0u16; NAME_CHARS];
    let mut len = NAME_CHARS as u32;
    // SAFETY: buffer is writable for len UTF-16 units and len points to its size.
    let ok = unsafe { GetUserNameW(buffer.as_mut_ptr(), &mut len) };
    if ok == 0 {
        return None;
    }
    // On success len counts the terminating zero.
    let end = (len as usize).saturating_sub(1).min(NAME_CHARS);
    let name = String::from_utf16_lossy(&buffer[..end]);
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

// The Settings switch "Animation effects". If the call fails, Windows' own
// default applies, which is on.
pub fn animations_on() -> bool {
    let mut on: BOOL = 1;
    // SAFETY: SPI_GETCLIENTAREAANIMATION writes a single BOOL to pvparam.
    let ok =
        unsafe { SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, (&raw mut on).cast(), 0) };
    ok == 0 || on != 0
}

// An invite carries its secret, and one made for anyone stays good for a
// day. Windows keeps plain copies in clipboard history and, with sync on,
// uploads them to the user's Microsoft account, so this copy is marked to
// skip both. Pasting it into a chat works as usual.
pub fn copy_private(text: &str) -> Result<(), arboard::Error> {
    use arboard::SetExtWindows;
    arboard::Clipboard::new()?
        .set()
        .exclude_from_history()
        .exclude_from_cloud()
        .text(text)
}

// The panel, just shown by a hotkey, in front of the game. Windows lets a
// program take the focus only in some cases, and a key heard through Raw
// Input may not be one of them; then the panel still comes to the top of the
// pile, and the game keeps the keyboard. egui's own Focus command is not
// used: winit fakes an Alt press with SendInput for it, and that would land
// in the game.
pub fn bring_forward(hwnd: isize) {
    let hwnd = hwnd as HWND;
    // SAFETY: the panel's own window, alive while the app runs; both calls
    // take plain values and fail harmlessly.
    unsafe {
        if SetForegroundWindow(hwnd) == 0 {
            SetWindowPos(
                hwnd,
                HWND_TOP,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
}

pub fn may_take_foreground() -> bool {
    // SAFETY: this process's own number tests the same foreground right it
    // will pass on to the panel, without changing which window has focus.
    unsafe { AllowSetForegroundWindow(GetCurrentProcessId()) != 0 }
}

pub fn take_foreground(hwnd: isize) -> bool {
    let hwnd = hwnd as HWND;
    // A link may come from a background program; without focus, placing the
    // panel above a game would put Join under a click meant for that game.
    // SAFETY: the panel's own window; a window that closed makes the calls fail.
    unsafe {
        if SetForegroundWindow(hwnd) == 0 {
            return false;
        }
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
    }
    true
}

// For winit's message hook, which sees each message the panel's thread takes
// off its queue before winit translates and dispatches it.
pub fn panel_key_hook(hook: Rc<RefCell<PanelHook>>) -> impl FnMut(*const c_void) -> bool + 'static {
    move |msg| {
        // SAFETY: winit hands its hook a pointer to the MSG it just took off
        // the queue, alive until the hook returns.
        let msg = unsafe { &*msg.cast::<MSG>() };
        hook.try_borrow_mut().is_ok_and(|mut hook| hook.keep(msg))
    }
}

// How long a key can wait in the queue before the panel reads it: far longer
// than a pass, and far shorter than the 49.7 days the clock takes to wrap.
const QUEUED_MS: u32 = 60_000;

// PanelKeys as the message hook has them. The hotkeys take every key pressed
// until the pass that ends a wait, and a key pressed while that pass runs is
// read after it, so PanelKeys go on waiting until a key message stamped
// later comes.
#[derive(Default)]
pub struct PanelHook {
    pub keys: PanelKeys,
    // The wait as the panel last set it.
    capture: bool,
    // When it ended, on the clock Windows stamps messages with, while
    // PanelKeys still wait.
    ended: Option<u32>,
}

impl PanelHook {
    // `now` is tick_count().
    pub fn set_capture(&mut self, on: bool, now: u32) {
        if on {
            self.ended = None;
            self.keys.set_capture(true);
        } else if self.capture {
            self.ended = Some(now);
        }
        self.capture = on;
    }

    // Only a key message ends the wait here: a posted message comes off the
    // queue ahead of keys pressed before it, whatever its time.
    pub fn keep(&mut self, msg: &MSG) -> bool {
        if let Some(ended) = self.ended
            && matches!(
                msg.message,
                WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP
            )
            && ended.wrapping_sub(msg.time) > QUEUED_MS
        {
            self.ended = None;
            self.keys.set_capture(false);
        }
        keep_from_panel(&mut self.keys, msg)
    }
}

// The clock Windows stamps messages with, in milliseconds since it started.
pub fn tick_count() -> u32 {
    // SAFETY: takes no arguments.
    unsafe { GetTickCount() }
}

// True keeps the message from the panel: a press never translated makes no
// text, paste or window menu either. Every key message this thread gets is
// the panel's: the region picker, the tray and the viewer have threads of
// their own.
pub fn keep_from_panel(keys: &mut PanelKeys, msg: &MSG) -> bool {
    if !matches!(
        msg.message,
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP
    ) {
        return false;
    }
    let bits = msg.lParam as u32;
    let Some(key) = Key::from_windows(
        msg.wParam as u32,
        (bits >> 16) & 0xFF,
        bits & (1 << 24) != 0,
    ) else {
        return false;
    };
    if matches!(msg.message, WM_KEYUP | WM_SYSKEYUP) {
        keys.release(key);
        return false;
    }
    let repeat = bits & (1 << 30) != 0;
    // SAFETY: both take a virtual key code and only read Windows' key state.
    let keyboard = || {
        readings(
            |vk| unsafe { GetKeyState(i32::from(vk)) } < 0,
            |vk| unsafe { GetAsyncKeyState(i32::from(vk)) } < 0,
        )
    };
    // The input method took this key, and egui-winit drops it; translating
    // it is how the input method's text arrives. The input method can let
    // the same key's repeats go by as the key itself, so a fresh press goes
    // to PanelKeys all the same.
    if msg.wParam == usize::from(VK_PROCESSKEY) {
        if !repeat {
            keys.press(key, &keyboard());
        }
        return false;
    }
    if repeat {
        return keys.repeat(key);
    }
    keys.press(key, &keyboard())
}

// GetKeyState follows the messages this thread has read and may lack a
// modifier pressed while another window had the focus; GetAsyncKeyState is
// the keyboard now, which a message read late can be behind.
fn readings(read: impl Fn(u16) -> bool, now: impl Fn(u16) -> bool) -> [Modifiers; 4] {
    let [read, read_altgr] = modifiers(read);
    let [now, now_altgr] = modifiers(now);
    [read, read_altgr, now, now_altgr]
}

// The modifiers held, and the same without Ctrl while Right Alt is down: on a
// layout with AltGr, Windows makes up a Left Ctrl press for Right Alt, and the
// hotkeys, which hear Raw Input only, never hear it.
fn modifiers(down: impl Fn(u16) -> bool) -> [Modifiers; 2] {
    let mut held = Modifiers::NONE;
    for (vk, modifier) in [
        (VK_SHIFT, Modifiers::SHIFT),
        (VK_MENU, Modifiers::ALT),
        (VK_LWIN, Modifiers::WIN),
        (VK_RWIN, Modifiers::WIN),
    ] {
        if down(vk) {
            held = held.with(modifier);
        }
    }
    let ctrl = held.with(Modifiers::CTRL);
    match (down(VK_CONTROL), down(VK_RMENU)) {
        (false, _) => [held; 2],
        (true, false) => [ctrl; 2],
        (true, true) => [ctrl, held],
    }
}

// The mouse now, whichever button is primary, as this goes by the physical
// buttons. Windows says up for all of them while the lock screen or an
// administrator prompt has the input, as that desktop is not this one.
pub fn mouse_button_down() -> bool {
    [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2]
        .into_iter()
        // SAFETY: takes a virtual key code and only reads Windows' key state.
        .any(|vk| unsafe { GetAsyncKeyState(i32::from(vk)) } < 0)
}

// The title bar in the window tone with chalk text, and the border in the
// same tone, so no grey band of Windows' own sits over the title row. Windows
// 10 does not know these and refuses them; it keeps the dark title bar winit
// asked for.
pub fn caption_colours(hwnd: isize) {
    for (attribute, colour) in [
        (DWMWA_CAPTION_COLOR, theme::WINDOW),
        (DWMWA_TEXT_COLOR, theme::CHALK),
        (DWMWA_BORDER_COLOR, theme::WINDOW),
    ] {
        dwm_colour(hwnd, attribute as u32, colour);
    }
}

// The caption buttons dim when the window loses the focus, and the title
// goes to ash with them, so with the panel and the viewer both open the
// title still says which one has the keyboard.
pub fn caption_text(hwnd: isize, focused: bool) {
    let colour = if focused { theme::CHALK } else { theme::ASH };
    dwm_colour(hwnd, DWMWA_TEXT_COLOR as u32, colour);
}

fn dwm_colour(hwnd: isize, attribute: u32, colour: Color32) {
    let value = u32::from(colour.r()) | u32::from(colour.g()) << 8 | u32::from(colour.b()) << 16;
    // SAFETY: the panel's own window, alive while the app runs, and a
    // COLORREF of the size passed.
    unsafe {
        DwmSetWindowAttribute(
            hwnd as HWND,
            attribute,
            ptr::from_ref(&value).cast(),
            size_of::<u32>() as u32,
        );
    }
}

// The panel's two window icons, small and big, as numbers. A window does not
// take over an icon it is sent, so they are kept until it is gone.
static WINDOW_ICONS: Mutex<[isize; 2]> = Mutex::new([0; 2]);

// The mark from booth.ico, the exe's icon resource 1 (build.rs), at the
// sizes this window's DPI asks for, so the title bar, the taskbar and
// Alt+Tab each show the frame drawn for their size rather than one picture
// scaled soft. Again whenever the DPI changes.
pub fn set_window_icons(hwnd: isize) {
    let hwnd = hwnd as HWND;
    // SAFETY: a plain query; it answers 0 for a window that is gone.
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    let mut held = WINDOW_ICONS.lock().unwrap_or_else(PoisonError::into_inner);
    for (slot, (kind, metric)) in [(ICON_SMALL, SM_CXSMICON), (ICON_BIG, SM_CXICON)]
        .into_iter()
        .enumerate()
    {
        // SAFETY: plain calls. Resource 1 of this exe is booth.ico; a build
        // without it, like a test, gets null and keeps what it had.
        let icon = unsafe {
            let size = GetSystemMetricsForDpi(metric, dpi);
            LoadImageW(
                GetModuleHandleW(ptr::null()),
                ptr::without_provenance(1),
                IMAGE_ICON,
                size,
                size,
                LR_DEFAULTCOLOR,
            )
        };
        if icon.is_null() {
            continue;
        }
        // SAFETY: the panel's own window and a live icon.
        unsafe { SendMessageW(hwnd, WM_SETICON, kind as usize, icon as isize) };
        destroy_icon(std::mem::replace(&mut held[slot], icon as isize));
    }
}

// Once the panel's window is gone.
pub fn drop_window_icons() {
    let mut held = WINDOW_ICONS.lock().unwrap_or_else(PoisonError::into_inner);
    for icon in held.iter_mut() {
        destroy_icon(std::mem::take(icon));
    }
}

fn destroy_icon(icon: isize) {
    if icon != 0 {
        // SAFETY: an icon LoadImageW made, which no window shows any more.
        unsafe { DestroyIcon(icon as _) };
    }
}

// A window winit drops is only posted its destroy, for winit's loop to read,
// and a panic leaves that loop first. Posted messages only: outside the loop
// winit asks again for every paint of a window still up, and the queue would
// never empty.
pub fn handle_posted_messages() {
    let mut msg = MSG::default();
    // SAFETY: `msg` is a live MSG for the call to write and is read only after
    // the call filled it in.
    unsafe {
        while PeekMessageW(
            &mut msg,
            ptr::null_mut(),
            0,
            0,
            PM_REMOVE | PM_QS_POSTMESSAGE,
        ) != 0
        {
            DispatchMessageW(&msg);
        }
    }
}

// For a run that is ending with something to say; a release build has no
// console to print to. On top, as a panel can stop behind a game, where Windows
// would not let a plain box come forward, or under the region picker's dimming.
pub fn error_box(text: &str) {
    let text: Vec<u16> = text.encode_utf16().chain([0]).collect();
    let title: Vec<u16> = "Booth".encode_utf16().chain([0]).collect();
    // SAFETY: both strings are zero terminated and live across the call.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR | MB_TOPMOST,
        );
    }
}

// Only the prompt gives the first two, and the Store's copy has no prompt.
#[cfg_attr(feature = "store", allow(dead_code))]
pub enum Elevated {
    Exited(u32),
    // The prompt was closed or answered No.
    Cancelled,
    Failed(io::Error),
}

// The Store's copy never asks for administrator rights; its package brings
// the firewall rule. Leaving the prompt out also keeps ShellExecuteEx, which
// the Store's checks flag as a way to launch programs, out of booth.exe.
#[cfg(not(feature = "store"))]
mod prompt;
#[cfg(not(feature = "store"))]
pub use prompt::run_elevated;

#[cfg(feature = "store")]
pub fn run_elevated(_exe: &Path, _argument: &str, _owner: Option<isize>) -> Elevated {
    Elevated::Failed(io::Error::other(
        "this copy is from the Microsoft Store, whose package brings the firewall rule",
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rights {
    // Started through the administrator prompt, or with Run as
    // administrator.
    Elevated,
    // An administrator with UAC off, or the built-in Administrator account:
    // every program runs with full rights and nothing asks.
    AlwaysFull,
    // An account that cannot become administrator at all. The prompt can
    // still take an administrator's password.
    Standard,
    // An administrator's everyday token while UAC is on, or Windows would
    // not say.
    Limited,
}

// While UAC is on Windows gives an administrator two tokens, one of them
// full, and anyone else one plain token. So a plain token outside the
// Administrators group is a standard user.
pub fn rights() -> Rights {
    let kind = elevation_type();
    if kind == Some(TokenElevationTypeFull) {
        Rights::Elevated
    } else if kind != Some(TokenElevationTypeDefault) {
        Rights::Limited
    } else if in_administrators() {
        Rights::AlwaysFull
    } else {
        Rights::Standard
    }
}

// For every run, before it does anything: what it loads by bare name from here
// on comes from System32, not from the current folder, PATH or the folder the
// exe sits in, which the user can write to. wgpu asks for d3d12.dll and
// dxcompiler.dll by bare name. System32 rarely has dxcompiler.dll, and without
// it wgpu uses FXC, as it already does on most PCs. One way round is left: for
// a WinRT class Windows does not have, windows-core passes its own flags and
// looks beside the exe. The exe's own imports are covered by build.rs, and
// FFmpeg is loaded by full path.
pub fn system_dlls_only() {
    // SAFETY: takes one flag and touches no memory of ours. It fails only
    // for flags Windows does not know, and this one it has known since 8.
    unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) };
}

fn elevation_type() -> Option<TOKEN_ELEVATION_TYPE> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: the pseudo handle for this process needs no closing, and
    // `token` is a live HANDLE for the call to write.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return None;
    }
    let mut kind: TOKEN_ELEVATION_TYPE = 0;
    let mut len = 0u32;
    // SAFETY: `kind` is writable for the size passed, and `token` is open
    // for query until the CloseHandle right after.
    let ok = unsafe {
        let ok = GetTokenInformation(
            token,
            TokenElevationType,
            (&raw mut kind).cast(),
            size_of::<TOKEN_ELEVATION_TYPE>() as u32,
            &mut len,
        );
        CloseHandle(token);
        ok
    };
    (ok != 0).then_some(kind)
}

// When Windows will not say, yes: the ordinary wording is the safer guess.
fn in_administrators() -> bool {
    // Held as u32s so the SID's own u32 fields are aligned.
    let mut sid = [0u32; SECURITY_MAX_SID_SIZE as usize / 4];
    let mut size = SECURITY_MAX_SID_SIZE;
    // SAFETY: `sid` is writable for `size` bytes, and a built-in group needs
    // no domain SID.
    let made = unsafe {
        CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            ptr::null_mut(),
            sid.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if made == 0 {
        return true;
    }
    let mut member: BOOL = 0;
    // SAFETY: a null token means this thread's own, `sid` holds the SID
    // written above, and `member` is a live BOOL for the answer.
    let ok = unsafe { CheckTokenMembership(ptr::null_mut(), sid.as_mut_ptr().cast(), &mut member) };
    ok == 0 || member != 0
}

// The form the room's log lines start with: "2026-09-24T21:12:03.456Z".
pub fn utc_stamp() -> String {
    let mut now = SYSTEMTIME::default();
    // SAFETY: GetSystemTime writes one SYSTEMTIME and cannot fail.
    unsafe { GetSystemTime(&mut now) };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        now.wYear, now.wMonth, now.wDay, now.wHour, now.wMinute, now.wSecond, now.wMilliseconds
    )
}

// Seconds since 1970 to 100 ns ticks since 1601, which is what FILETIME is.
const FILETIME_OFFSET_SECS: u64 = 11_644_473_600;

// The date on this PC's calendar, with the daylight saving rule of that day,
// as (year, month, day). None when Windows cannot say.
pub fn local_date(unix: u64) -> Option<(u16, u16, u16)> {
    local(unix).map(|local| (local.wYear, local.wMonth, local.wDay))
}

// The time of day on this PC's clock, as (hour, minute), by the same rule.
pub fn local_time(unix: u64) -> Option<(u16, u16)> {
    local(unix).map(|local| (local.wHour, local.wMinute))
}

fn local(unix: u64) -> Option<SYSTEMTIME> {
    let ticks = unix
        .checked_add(FILETIME_OFFSET_SECS)?
        .checked_mul(10_000_000)?;
    let file = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    // SAFETY: each call reads one initialized struct and writes one it was
    // handed a pointer to; both live across the calls. A null time zone
    // means the one this PC is set to.
    let ok = unsafe {
        FileTimeToSystemTime(&file, &mut utc) != 0
            && SystemTimeToTzSpecificLocalTime(ptr::null(), &utc, &mut local) != 0
    };
    ok.then_some(local)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{RDW_INTERNALPAINT, RedrawWindow};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        VK_BACK, VK_ESCAPE, VK_F10, VK_LMENU, VK_M, VK_PACKET, VK_RETURN, VK_SPACE,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, HWND_MESSAGE, IsWindow, PostMessageW, RegisterClassExW,
        WM_CHAR, WM_CLOSE, WM_PAINT, WNDCLASSEXW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
        WS_VISIBLE,
    };

    use super::*;

    // In lParam, beside the scan code: E0, and the key was down before.
    const EXTENDED: u32 = 1 << 24;
    const AGAIN: u32 = 1 << 30;

    fn message(message: u32, vk: u16, scan: u32, bits: u32) -> MSG {
        MSG {
            message,
            wParam: usize::from(vk),
            lParam: ((scan << 16) | bits | 1) as isize,
            ..MSG::default()
        }
    }

    fn down(vk: u16, scan: u32, bits: u32) -> MSG {
        message(WM_KEYDOWN, vk, scan, bits)
    }

    fn up(vk: u16, scan: u32) -> MSG {
        message(WM_KEYUP, vk, scan, AGAIN | (1 << 31))
    }

    // A window asked to go by a posted message, as winit does it, with no
    // loop left to read it. A message-only window never shows.
    #[test]
    fn a_window_posted_its_end_goes() {
        let class: Vec<u16> = "STATIC".encode_utf16().chain([0]).collect();
        // SAFETY: a zero terminated class name that lives across the call, a
        // parent Windows defines, and null for everything else.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                ptr::null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        assert!(!hwnd.is_null(), "no window");
        // SAFETY: this thread's own window, and a message with no pointers.
        unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) };
        // SAFETY: a plain query; it answers 0 for a window that is gone.
        assert_ne!(unsafe { IsWindow(hwnd) }, 0, "gone before anything read it");
        handle_posted_messages();
        // SAFETY: as above.
        assert_eq!(unsafe { IsWindow(hwnd) }, 0, "the window is still up");
    }

    // A window still up, doing what winit does with a paint outside its loop.
    // Shown, as Windows paints nothing else, but with no size and never active.
    // On a thread of its own, so a drain that never returns fails the test
    // rather than hanging it.
    #[test]
    fn a_window_asking_for_paints_does_not_hold_the_drain() {
        unsafe extern "system" fn window_proc(
            hwnd: HWND,
            message: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            // SAFETY: the arguments are the ones Windows called with.
            let result = unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
            if message == WM_PAINT {
                // SAFETY: this thread's own window, and no rectangle or region.
                unsafe { RedrawWindow(hwnd, ptr::null(), ptr::null_mut(), RDW_INTERNALPAINT) };
            }
            result
        }
        let (drained, was_drained) = mpsc::channel();
        thread::spawn(move || {
            let class: Vec<u16> = "BoothPaintsAgain".encode_utf16().chain([0]).collect();
            // SAFETY: a null name asks for this exe's own module handle, which
            // needs no closing.
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
            assert_ne!(unsafe { RegisterClassExW(&info) }, 0, "no class");
            // SAFETY: the class was registered above with this module, the
            // name is zero terminated, and every other pointer is null.
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                    class.as_ptr(),
                    ptr::null(),
                    WS_POPUP | WS_VISIBLE,
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
            assert!(!hwnd.is_null(), "no window");
            // SAFETY: this thread's own window, and no rectangle or region.
            let asked =
                unsafe { RedrawWindow(hwnd, ptr::null(), ptr::null_mut(), RDW_INTERNALPAINT) };
            assert_ne!(asked, 0, "no paint asked for");
            handle_posted_messages();
            drained.send(()).expect("the test is waiting");
        });
        assert!(
            was_drained.recv_timeout(Duration::from_secs(10)).is_ok(),
            "the drain did not return"
        );
    }

    // No bindings, so the hotkeys take no press and the keyboard's real
    // state, which a fresh press reads, never decides here.
    #[test]
    fn key_messages_reach_panel_keys() {
        let mut keys = PanelKeys::default();
        let steps = [
            ("Space held from before", down(VK_SPACE, 0x39, AGAIN), true),
            ("its release", up(VK_SPACE, 0x39), false),
            ("a character", message(WM_CHAR, 0x20, 0x39, AGAIN), false),
            ("Space pressed", down(VK_SPACE, 0x39, 0), false),
            ("its repeat", down(VK_SPACE, 0x39, AGAIN), false),
            ("its release", up(VK_SPACE, 0x39), false),
            ("Space pressed again", down(VK_SPACE, 0x39, 0), false),
            (
                "its release with Alt down",
                message(WM_SYSKEYUP, VK_SPACE, 0x39, AGAIN | 1 << 31 | 1 << 29),
                false,
            ),
            (
                "Space held from before, after it",
                down(VK_SPACE, 0x39, AGAIN),
                true,
            ),
            (
                "F10 held from before",
                message(WM_SYSKEYDOWN, VK_F10, 0x44, AGAIN),
                true,
            ),
            (
                "Right Ctrl held from before",
                down(VK_CONTROL, 0x1D, EXTENDED | AGAIN),
                false,
            ),
            ("a packet", down(VK_PACKET, 0, 0), false),
            (
                "the input method's",
                down(VK_PROCESSKEY, 0x39, AGAIN),
                false,
            ),
            (
                "Space still held from before",
                down(VK_SPACE, 0x39, AGAIN),
                true,
            ),
            (
                "Backspace to the input method",
                down(VK_PROCESSKEY, 0x0E, 0),
                false,
            ),
            ("its repeat as itself", down(VK_BACK, 0x0E, AGAIN), false),
            ("Enter pressed", down(VK_RETURN, 0x1C, 0), false),
            (
                "Num Enter held from before",
                down(VK_RETURN, 0x1C, EXTENDED | AGAIN),
                true,
            ),
        ];
        for (what, msg, kept) in steps {
            assert_eq!(keep_from_panel(&mut keys, &msg), kept, "{what}");
        }
        keys.set_capture(true);
        assert!(
            keep_from_panel(&mut keys, &down(VK_M, 0x32, 0)),
            "M while Settings waits"
        );
        assert!(
            !keep_from_panel(&mut keys, &down(VK_PROCESSKEY, 0x32, 0)),
            "the input method's key while Settings waits"
        );
    }

    fn at(time: u32, msg: MSG) -> MSG {
        MSG { time, ..msg }
    }

    // The hotkeys can hear a key end the wait, and the pass that takes it
    // from them end it here, before this thread reads that key's message.
    // Each key pressed until then is the wait's, read before or after.
    #[test]
    fn keys_pressed_before_the_wait_ended_stay_the_waits() {
        let mut hook = PanelHook::default();
        hook.set_capture(true, 1000);
        hook.set_capture(false, 1010);
        // The same pass looks again once ui has run.
        hook.set_capture(false, 1020);
        let steps = [
            (
                "a posted message",
                at(1030, message(WM_CLOSE, 0, 0, 0)),
                false,
            ),
            ("Esc pressed", at(1005, down(VK_ESCAPE, 0x01, 0)), true),
            ("Space pressed", at(1010, down(VK_SPACE, 0x39, 0)), true),
            (
                "M pressed after the first look",
                at(1015, down(VK_M, 0x32, 0)),
                false,
            ),
            ("Esc let go", at(1040, up(VK_ESCAPE, 0x01)), false),
            (
                "Space repeating",
                at(1045, down(VK_SPACE, 0x39, AGAIN)),
                true,
            ),
            (
                "Esc pressed again",
                at(1050, down(VK_ESCAPE, 0x01, 0)),
                false,
            ),
            ("its repeat", at(1080, down(VK_ESCAPE, 0x01, AGAIN)), false),
        ];
        for (what, msg, kept) in steps {
            assert_eq!(hook.keep(&msg), kept, "{what}");
        }

        // A wait that ended just after the clock wrapped.
        hook.set_capture(true, 1100);
        hook.set_capture(false, 20);
        let before_the_wrap = at(u32::MAX - 5, down(VK_RETURN, 0x1C, 0));
        assert!(hook.keep(&before_the_wrap), "Enter before the wrap");
        // A wait can end with no key after it, and the next one's keys are
        // its own all the same.
        hook.set_capture(true, 40);
        let next = at(45, down(VK_RETURN, 0x1C, 0));
        assert!(hook.keep(&next), "Enter for the next wait");
        // Weeks after a wait, when the clock has gone round far enough that
        // its end looks still to come, a key goes to the panel.
        hook.set_capture(false, 50);
        let long_after = at(50 + (1 << 31) + 10, down(VK_RETURN, 0x1C, 0));
        assert!(!hook.keep(&long_after), "Enter weeks later");
    }

    #[test]
    fn altgr_is_read_with_and_without_its_ctrl() {
        let read = |held: &[u16]| modifiers(|vk| held.contains(&vk));
        let ctrl_alt = Modifiers::CTRL.with(Modifiers::ALT);
        assert_eq!(read(&[]), [Modifiers::NONE; 2]);
        assert_eq!(
            read(&[VK_CONTROL, VK_SHIFT]),
            [Modifiers::CTRL.with(Modifiers::SHIFT); 2]
        );
        assert_eq!(read(&[VK_CONTROL, VK_MENU, VK_LMENU]), [ctrl_alt; 2]);
        assert_eq!(
            read(&[VK_CONTROL, VK_MENU, VK_RMENU]),
            [ctrl_alt, Modifiers::ALT]
        );
        assert_eq!(read(&[VK_MENU, VK_RMENU]), [Modifiers::ALT; 2]);
        let altgr = |vk: u16| [VK_CONTROL, VK_MENU, VK_RMENU].contains(&vk);
        let up = |_: u16| false;
        for four in [readings(altgr, up), readings(up, altgr)] {
            assert!(four.contains(&ctrl_alt), "{four:?}");
            assert!(four.contains(&Modifiers::ALT), "{four:?}");
        }
    }
}
