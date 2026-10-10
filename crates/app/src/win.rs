use std::io;
#[cfg(feature = "store")]
use std::path::Path;
use std::ptr;
use std::sync::{Mutex, PoisonError};

use eframe::egui::Color32;
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
use windows_sys::Win32::System::SystemInformation::GetSystemTime;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows_sys::Win32::System::WindowsProgramming::GetUserNameW;
use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, HWND_TOP, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTCOLOR, LoadImageW,
    MB_ICONERROR, MB_OK, MessageBoxW, SM_CXICON, SM_CXSMICON, SPI_GETCLIENTAREAANIMATION,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SendMessageW, SetForegroundWindow, SetWindowPos,
    SystemParametersInfoW, WM_SETICON,
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

// Only for a window that could not open, or a start the arguments stopped;
// a release build has no console to print to.
pub fn error_box(text: &str) {
    let text: Vec<u16> = text.encode_utf16().chain([0]).collect();
    let title: Vec<u16> = "Booth".encode_utf16().chain([0]).collect();
    // SAFETY: both strings are zero terminated and live across the call.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
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

// For the elevated helper, before it does anything: what it loads from here
// on comes from System32, never from the folder the exe sits in, which the
// user can write to. The exe's own imports are covered by build.rs.
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
