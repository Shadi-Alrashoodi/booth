// The windows a share can show, as the picker lists them: the ones the
// taskbar would give a button to, other programs' only, none that keeps
// itself out of captures. A title is for the picker alone: it never goes
// into a message, the log or the settings, so Debug leaves it out.

use std::collections::HashMap;
use std::fmt;
use std::mem;

use windows::Graphics::Capture::GraphicsCaptureSession;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, TRUE};
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, QueryDosDeviceW, VerQueryValueW,
};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GWL_STYLE, GetShellWindow, GetWindow,
    GetWindowDisplayAffinity, GetWindowLongPtrW, GetWindowTextW, IsWindowVisible,
    WDA_EXCLUDEFROMCAPTURE, WDA_MONITOR, WS_CHILD, WS_EX_APPWINDOW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW,
};
use windows::core::{BOOL, HSTRING, PCWSTR, PWSTR, w};

use crate::error::CaptureError;
use crate::pointer::frame_bounds;
use crate::window::{WindowId, captures_windows, keep_apartment};

// Longer titles are cut here; the picker cuts them shorter still.
const LONGEST_TITLE: usize = 256;

#[derive(Clone, PartialEq, Eq)]
pub struct TopWindow {
    pub id: WindowId,
    pub title: String,
    // The program's display name, or its file name without ".exe", which
    // tells two windows of the same title apart; None for a hosted app's
    // frame or when Windows will not say.
    pub program: Option<String>,
    pub minimized: bool,
}

impl fmt::Debug for TopWindow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TopWindow")
            .field("id", &self.id)
            .field("title_chars", &self.title.chars().count())
            .field("program", &self.program)
            .field("minimized", &self.minimized)
            .finish()
    }
}

// In the order Windows keeps them, front to back as it happens. Empty on a
// PC whose Windows cannot capture a single window.
pub fn top_windows() -> Result<Vec<TopWindow>, CaptureError> {
    keep_apartment();
    if !captures_windows(GraphicsCaptureSession::IsSupported())? {
        return Ok(Vec::new());
    }
    let mut handles: Vec<HWND> = Vec::new();
    // SAFETY: the callback only pushes into the Vec whose address it is
    // given, which outlives the call.
    unsafe {
        EnumWindows(
            Some(collect),
            LPARAM(&mut handles as *mut Vec<HWND> as isize),
        )
    }
    .map_err(|err| CaptureError::windows("list the windows", &err))?;
    // SAFETY: plain queries.
    let (own, shell) = unsafe { (GetCurrentProcessId(), GetShellWindow()) };
    let mut programs = HashMap::new();
    Ok(handles
        .into_iter()
        .filter_map(|hwnd| listed(hwnd, own, shell, &mut programs))
        .collect())
}

// SAFETY: called by EnumWindows with the LPARAM top_windows passed, a live
// Vec<HWND>. Nothing here can panic.
unsafe extern "system" fn collect(hwnd: HWND, list: LPARAM) -> BOOL {
    // SAFETY: as the function's contract says.
    unsafe { (*(list.0 as *mut Vec<HWND>)).push(hwnd) };
    TRUE
}

// The taskbar's rule, then what a capture needs. Booth's own windows go
// first, before any text is read: reading the title of a window of this
// process sends it a message, and a window whose thread is stuck would hold
// the panel up.
fn listed(
    hwnd: HWND,
    own: u32,
    shell: HWND,
    programs: &mut HashMap<String, Option<String>>,
) -> Option<TopWindow> {
    // SAFETY: plain queries; a window gone since it was listed answers 0
    // or false, and is left out.
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || hwnd == shell {
            return None;
        }
        if GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 & WS_CHILD.0 != 0 {
            return None;
        }
    }
    let id = WindowId::of(hwnd.0 as isize)?;
    if id.process == own {
        return None;
    }
    // SAFETY: as above.
    let extended = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    if extended & WS_EX_TOOLWINDOW.0 != 0 {
        return None;
    }
    // SAFETY: as above; no owner answers an error.
    let owned = unsafe { GetWindow(hwnd, GW_OWNER) }.is_ok();
    if extended & WS_EX_APPWINDOW.0 == 0 && (owned || extended & WS_EX_NOACTIVATE.0 != 0) {
        return None;
    }
    if cloaked(hwnd) || kept_out(hwnd) {
        return None;
    }
    // Zero-size helpers some programs keep visible.
    if frame_bounds(id.hwnd)
        .is_some_and(|(left, top, right, bottom)| right - left < 2 || bottom - top < 2)
    {
        return None;
    }
    let title = title(hwnd);
    if title.is_empty() {
        return None;
    }
    Some(TopWindow {
        id,
        title,
        program: program(id.process, programs),
        minimized: id.minimized(),
    })
}

// On another virtual desktop, or hidden by its program or the shell: not a
// window the person sees. A failed read counts as not cloaked.
fn cloaked(hwnd: HWND) -> bool {
    let mut cloaked = 0u32;
    // SAFETY: a live u32 of the size passed.
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut _,
            mem::size_of::<u32>() as u32,
        )
    }
    .is_ok()
        && cloaked != 0
}

// Its program asked Windows to keep it out of captures: a share of it would
// show nothing. Windows answers for some windows only, and no answer lists
// the window.
fn kept_out(hwnd: HWND) -> bool {
    let mut affinity = 0u32;
    // SAFETY: a live u32.
    unsafe { GetWindowDisplayAffinity(hwnd, &mut affinity) }.is_ok()
        && (affinity == WDA_MONITOR.0 || affinity == WDA_EXCLUDEFROMCAPTURE.0)
}

// The title as the picker shows it: no control characters, no characters
// that turn the text's direction or take no space, trimmed and cut.
fn title(hwnd: HWND) -> String {
    let mut text = [0u16; 512];
    // SAFETY: a live buffer of the length passed; another program's window
    // is not sent anything, its stored title is read.
    let length = unsafe { GetWindowTextW(hwnd, &mut text) };
    let text = String::from_utf16_lossy(&text[..length.max(0) as usize]);
    clean_title(&text)
}

fn clean_title(text: &str) -> String {
    let dropped = |c: char| {
        c.is_control()
            || matches!(
                c,
                '\u{061C}'
                    | '\u{200B}'
                    | '\u{200E}'
                    | '\u{200F}'
                    | '\u{202A}'..='\u{202E}'
                    | '\u{2060}'
                    | '\u{2066}'..='\u{2069}'
                    | '\u{FEFF}'
            )
    };
    let kept: String = text.chars().filter(|&c| !dropped(c)).collect();
    kept.trim().chars().take(LONGEST_TITLE).collect()
}

// A listing reads each file once, even when several windows belong to it
// or its version resource cannot be read. The picker keeps these names
// until it takes a new listing.
fn program(process: u32, programs: &mut HashMap<String, Option<String>>) -> Option<String> {
    // SAFETY: a plain open for the least access there is; closed below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process) }.ok()?;
    let mut path = [0u16; 1024];
    let mut length = path.len() as u32;
    // SAFETY: a live handle and a live buffer whose length is passed.
    let read = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut length,
        )
    };
    // SAFETY: the handle opened above, closed once.
    let _ = unsafe { CloseHandle(handle) };
    read.ok()?;
    let path = String::from_utf16_lossy(&path[..length as usize]);
    programs
        .entry(path)
        .or_insert_with_key(|path| program_name(path))
        .clone()
}

fn program_name(path: &str) -> Option<String> {
    program_name_from(path, device_target, file_description)
}

fn program_name_from(
    path: &str,
    device: impl Fn(&str) -> Option<String>,
    description: impl FnOnce(&str) -> Option<String>,
) -> Option<String> {
    let name = path.rsplit(['\\', '/']).next()?;
    if name.eq_ignore_ascii_case("ApplicationFrameHost.exe") {
        return None;
    }
    if on_local_disk(path, device)
        && let Some(description) = description(path)
    {
        return Some(description);
    }
    let stem = name
        .strip_suffix(".exe")
        .or_else(|| name.strip_suffix(".EXE"))
        .unwrap_or(name);
    let stem = clean_title(stem);
    (!stem.is_empty()).then_some(stem)
}

// Opening a file on a dropped share can hold up the picker, and network,
// WebDAV and shared-folder drives each name a device of their own, so a
// program's file is read only from a local disk volume. The device mapping
// is read without opening the volume. A substituted drive names the folder
// it stands for, and that folder's drive is asked about once more.
fn on_local_disk(path: &str, device: impl Fn(&str) -> Option<String>) -> bool {
    let target = |path: &str| {
        path.get(..2)
            .filter(|drive| drive.ends_with(':'))
            .and_then(&device)
    };
    let disk = |target: &str| {
        target
            .to_ascii_lowercase()
            .strip_prefix(r"\device\harddiskvolume")
            .is_some_and(|number| {
                !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
            })
    };
    target(path).is_some_and(|first| {
        disk(&first)
            || first
                .strip_prefix(r"\??\")
                .and_then(&target)
                .is_some_and(|second| disk(&second))
    })
}

fn device_target(drive: &str) -> Option<String> {
    let drive = HSTRING::from(drive);
    let mut target = [0u16; 1024];
    // SAFETY: a live, terminated drive name and a live output buffer.
    let length = unsafe { QueryDosDeviceW(PCWSTR(drive.as_ptr()), Some(&mut target)) };
    if length == 0 {
        return None;
    }
    let target = target[..length as usize].split(|&unit| unit == 0).next()?;
    Some(String::from_utf16_lossy(target))
}

fn version_words(info: &[u32], value: *const std::ffi::c_void, length: usize) -> &[u16] {
    let Some(offset) = (value as usize).checked_sub(info.as_ptr() as usize) else {
        return &[];
    };
    let bytes = mem::size_of_val(info);
    if offset >= bytes || offset % mem::align_of::<u16>() != 0 {
        return &[];
    }
    let length = length.min((bytes - offset) / mem::size_of::<u16>());
    // SAFETY: info is aligned for u16 and fully initialized, and every u16
    // value is valid. The returned slice is cut within that buffer.
    let words = unsafe {
        std::slice::from_raw_parts(info.as_ptr().cast::<u16>(), bytes / mem::size_of::<u16>())
    };
    &words[offset / mem::size_of::<u16>()..][..length]
}

fn version_description(
    info: &[u32],
    value: *const std::ffi::c_void,
    length: usize,
) -> Option<String> {
    let text = version_words(info, value, length);
    let end = text
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(text.len());
    let description = clean_title(&String::from_utf16_lossy(&text[..end]));
    (!description.is_empty()).then_some(description)
}

fn translation_order(translations: &[[u16; 2]], language: u16) -> impl Iterator<Item = [u16; 2]> {
    translations
        .iter()
        .filter(move |entry| entry[0] == language)
        .chain(
            translations
                .iter()
                .filter(move |entry| entry[0] != language),
        )
        .copied()
        .chain([[0x0409, 0x04B0], [0x0409, 0x04E4]])
}

fn file_description(path: &str) -> Option<String> {
    let path = HSTRING::from(path);
    // SAFETY: a live, terminated path; the unused handle is optional.
    let size = unsafe { GetFileVersionInfoSizeW(PCWSTR(path.as_ptr()), None) };
    if size == 0 {
        return None;
    }
    // The translation pairs and strings returned below need word alignment.
    let mut info = vec![0u32; (size as usize).div_ceil(mem::size_of::<u32>())];
    // SAFETY: a live path and a buffer at least size bytes long.
    unsafe { GetFileVersionInfoW(PCWSTR(path.as_ptr()), None, size, info.as_mut_ptr().cast()) }
        .ok()?;
    let mut value = std::ptr::null_mut();
    let mut length = 0;
    // SAFETY: the complete version block and live output pointers.
    let translated = unsafe {
        VerQueryValueW(
            info.as_ptr().cast(),
            w!("\\VarFileInfo\\Translation"),
            &mut value,
            &mut length,
        )
    };
    let translations: Vec<[u16; 2]> = if translated.as_bool() {
        version_words(&info, value, length as usize / mem::size_of::<u16>())
            .as_chunks::<2>()
            .0
            .to_vec()
    } else {
        Vec::new()
    };
    // SAFETY: a plain query.
    let language = unsafe { GetUserDefaultUILanguage() };
    for [language, codepage] in translation_order(&translations, language) {
        let key = HSTRING::from(format!(
            "\\StringFileInfo\\{language:04X}{codepage:04X}\\FileDescription"
        ));
        // SAFETY: the complete version block, a live terminated key and
        // live output pointers. String lengths are in UTF-16 code units.
        let found = unsafe {
            VerQueryValueW(
                info.as_ptr().cast(),
                PCWSTR(key.as_ptr()),
                &mut value,
                &mut length,
            )
        };
        if found.as_bool()
            && let Some(description) = version_description(&info, value, length as usize)
        {
            return Some(description);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAK, DwmSetWindowAttribute};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DispatchMessageW, IsWindow, MSG, PM_REMOVE, PeekMessageW, PostMessageW,
        SW_SHOWMINNOACTIVE, SetWindowDisplayAffinity, ShowWindow, TranslateMessage,
        WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WS_OVERLAPPEDWINDOW, WS_POPUP, WS_VISIBLE,
    };
    use windows::core::{HSTRING, PCWSTR, w};

    use super::*;

    #[test]
    fn version_values_stay_within_the_block() {
        let storage = [0x00420041u32, 0x00440043, 0x00460045];
        let info = &storage[..2];
        let start = info.as_ptr().cast::<u8>();
        assert_eq!(
            version_words(info, start.wrapping_add(2).cast(), 4),
            [66, 67, 68]
        );
        assert_eq!(version_words(info, start.cast(), 1), [65]);
        for value in [
            std::ptr::null(),
            start.wrapping_sub(2).cast(),
            start.wrapping_add(1).cast(),
            start.wrapping_add(mem::size_of_val(info)).cast(),
            start.wrapping_add(mem::size_of_val(info) + 2).cast(),
        ] {
            assert!(version_words(info, value, 4).is_empty());
        }
        assert!(version_words(info, start.cast(), 0).is_empty());
    }

    #[test]
    fn a_version_description_ends_at_its_first_nul() {
        let info = [0x00420041u32, 0x00430000, 0x00450044];
        assert_eq!(
            version_description(&info, info.as_ptr().cast(), 6).as_deref(),
            Some("AB")
        );
        assert_eq!(
            version_description(&info, info.as_ptr().cast(), 1).as_deref(),
            Some("A")
        );
        let empty = [0u32];
        assert_eq!(version_description(&empty, empty.as_ptr().cast(), 2), None);
    }

    #[test]
    fn translations_for_the_ui_language_come_first() {
        let translations = [[0x0409, 0x04E4], [0x0401, 0x04B0], [0x0411, 0x03A4]];
        assert_eq!(
            translation_order(&translations, 0x0401).collect::<Vec<_>>(),
            [
                [0x0401, 0x04B0],
                [0x0409, 0x04E4],
                [0x0411, 0x03A4],
                [0x0409, 0x04B0],
                [0x0409, 0x04E4]
            ]
        );
    }

    #[test]
    fn translations_keep_the_file_order_when_no_language_matches() {
        let translations = [[0x0411, 0x03A4], [0x0401, 0x04B0]];
        assert_eq!(
            translation_order(&translations, 0x0419).collect::<Vec<_>>(),
            [
                [0x0411, 0x03A4],
                [0x0401, 0x04B0],
                [0x0409, 0x04B0],
                [0x0409, 0x04E4]
            ]
        );
    }

    #[test]
    fn missing_translations_use_the_english_fallbacks() {
        assert_eq!(
            translation_order(&[], 0x0401).collect::<Vec<_>>(),
            [[0x0409, 0x04B0], [0x0409, 0x04E4]]
        );
    }

    #[test]
    fn remote_programs_use_the_file_name_without_reading_the_file() {
        for path in [
            r"\\server\apps\Notes.exe",
            r"\\?\UNC\server\apps\Notes.exe",
            "//server/apps/Notes.exe",
        ] {
            assert_eq!(
                program_name_from(
                    path,
                    |_| panic!("a UNC path needs no drive query"),
                    |_| panic!("a remote file must not be opened")
                )
                .as_deref(),
                Some("Notes")
            );
        }
        for drives in [
            &[("Z:", r"\Device\Mup")][..],
            &[("Z:", r"\Device\Mup\server\apps")],
            &[(
                "Z:",
                r"\Device\LanmanRedirector\;Z:000000000000\server\apps",
            )],
            &[("Z:", r"\device\lanmanredirector\server\apps")],
            &[("Z:", r"\??\UNC\server\apps")],
            &[(
                "Z:",
                r"\Device\WebDavRedirector\;Z:0000000000000000\server\DavWWWRoot",
            )],
            &[("Z:", r"\Device\VBoxMiniRdr\;Z:\VBoxSvr\apps")],
            &[("Z:", r"\Device\HarddiskVolume")],
            &[
                ("Z:", r"\??\Y:\apps"),
                ("Y:", r"\Device\LanmanRedirector\;Y:0\server\apps"),
            ],
            &[("Z:", r"\??\Y:\apps")],
            &[],
        ] {
            assert_eq!(
                program_name_from(
                    r"Z:\Notes.exe",
                    |drive| mapped(drives, drive),
                    |_| panic!("a remote file must not be opened")
                )
                .as_deref(),
                Some("Notes"),
                "{drives:?}"
            );
        }
    }

    fn mapped(drives: &[(&str, &str)], drive: &str) -> Option<String> {
        drives
            .iter()
            .find(|(name, _)| *name == drive)
            .map(|(_, target)| target.to_string())
    }

    #[test]
    fn local_programs_can_use_the_file_description() {
        for drives in [
            &[("C:", r"\Device\HarddiskVolume4")][..],
            &[("C:", r"\device\harddiskvolume12")],
            &[("C:", r"\??\D:\apps"), ("D:", r"\Device\HarddiskVolume2")],
        ] {
            assert_eq!(
                program_name_from(
                    r"C:\Notes.exe",
                    |drive| mapped(drives, drive),
                    |path| {
                        assert_eq!(path, r"C:\Notes.exe");
                        Some(String::from("Notes display name"))
                    }
                )
                .as_deref(),
                Some("Notes display name")
            );
        }
    }

    #[test]
    fn file_name_fallbacks_drop_direction_controls_and_empty_names() {
        for (stem, expected) in [
            (" \u{2069}Notes\u{202E}\u{2066} \u{2069}", Some("Notes")),
            ("\u{2066}\u{202E}\u{2069}", None),
        ] {
            let path = format!(r"C:\apps\{stem}.exe");
            assert_eq!(
                program_name_from(
                    &path,
                    |_| Some(String::from(r"\Device\HarddiskVolume4")),
                    |_| None
                )
                .as_deref(),
                expected
            );
        }
    }

    #[test]
    fn notepad_uses_its_file_description() {
        let path = r"C:\Windows\System32\notepad.exe";
        let description = file_description(path).expect("Notepad has a version resource");
        assert_ne!(description, "notepad");
        assert_eq!(program_name(path), Some(description));
    }

    #[test]
    fn a_program_without_a_version_resource_uses_its_file_name() {
        let path = std::env::current_exe().unwrap();
        let name = path.file_stem().unwrap().to_str().unwrap();
        let path = path.to_str().unwrap();
        assert_eq!(file_description(path), None);
        assert_eq!(program_name(path).as_deref(), Some(name));
    }

    #[test]
    fn a_hosted_apps_frame_has_no_program_name() {
        for path in [
            r"C:\Windows\System32\ApplicationFrameHost.exe",
            r"C:\Windows\System32\APPLICATIONFRAMEHOST.EXE",
        ] {
            assert_eq!(program_name(path), None);
        }
    }

    #[test]
    fn program_names_are_kept_by_full_path_including_missing_names() {
        // SAFETY: a plain query.
        let process = unsafe { GetCurrentProcessId() };
        let mut programs = HashMap::new();
        let first = program(process, &mut programs).expect("this executable has a file name");
        assert_eq!(programs.len(), 1);
        let path = programs.keys().next().unwrap().clone();
        assert!(std::path::Path::new(&path).is_absolute());
        assert_eq!(programs[&path].as_deref(), Some(first.as_str()));
        programs.insert(path.clone(), Some(String::from("kept name")));
        assert_eq!(
            program(process, &mut programs).as_deref(),
            Some("kept name")
        );
        programs.insert(path, None);
        assert_eq!(program(process, &mut programs), None);
        assert_eq!(programs.len(), 1);
    }

    // One kind of window the list is asked about.
    struct Kind {
        name: &'static str,
        title: &'static str,
        style: WINDOW_STYLE,
        extended: WINDOW_EX_STYLE,
        owned: bool,
        then: fn(HWND),
    }

    fn nothing(_: HWND) {}

    // The windows, all on one thread that pumps their messages, the first
    // one the owner of the owned ones; closed when the guard goes.
    struct Made(Vec<(&'static str, HWND)>);

    impl Drop for Made {
        fn drop(&mut self) {
            for (_, hwnd) in &self.0 {
                // SAFETY: posted to the windows' own thread.
                let _ = unsafe { PostMessageW(Some(*hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
            }
        }
    }

    fn make(kinds: Vec<Kind>) -> Made {
        let (sent, got) = mpsc::channel();
        thread::spawn(move || {
            let mut made: Vec<(&'static str, HWND)> = Vec::new();
            for kind in &kinds {
                let owner = if kind.owned { Some(made[0].1) } else { None };
                let title = HSTRING::from(kind.title);
                // SAFETY: a system class, live strings, an owner of this
                // thread or none.
                let hwnd = unsafe {
                    CreateWindowExW(
                        kind.extended,
                        w!("STATIC"),
                        PCWSTR(title.as_ptr()),
                        kind.style | WS_VISIBLE,
                        200,
                        200,
                        320,
                        200,
                        owner,
                        None,
                        None,
                        None,
                    )
                }
                .unwrap();
                (kind.then)(hwnd);
                made.push((kind.name, hwnd));
            }
            let hwnds: Vec<(&'static str, isize)> = made
                .iter()
                .map(|(name, hwnd)| (*name, hwnd.0 as isize))
                .collect();
            sent.send(hwnds).unwrap();
            let mut msg = MSG::default();
            // SAFETY: windows of this thread; gone ones answer false.
            while made
                .iter()
                .any(|(_, hwnd)| unsafe { IsWindow(Some(*hwnd)) }.as_bool())
            {
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
        let hwnds = got.recv().unwrap();
        Made(
            hwnds
                .into_iter()
                .map(|(name, hwnd)| (name, HWND(hwnd as *mut _)))
                .collect(),
        )
    }

    // The taskbar's rule and capture's: which of this test's own windows
    // would be listed if they were another program's, and how.
    #[test]
    fn the_list_follows_the_taskbar() {
        if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
            println!("skipped: this Windows cannot capture a single window");
            return;
        }
        let plain = |name, title| Kind {
            name,
            title,
            style: WS_OVERLAPPEDWINDOW,
            extended: WINDOW_EX_STYLE(0),
            owned: false,
            then: nothing,
        };
        let minimize = |hwnd: HWND| {
            // SAFETY: a window of this thread.
            let _ = unsafe { ShowWindow(hwnd, SW_SHOWMINNOACTIVE) };
        };
        let exclude = |hwnd: HWND| {
            // SAFETY: a window of this process.
            let _ = unsafe { SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) };
        };
        let cloak = |hwnd: HWND| {
            let on = BOOL::from(true);
            // SAFETY: a live BOOL of the size passed, a window of this
            // process.
            let _ = unsafe {
                DwmSetWindowAttribute(
                    hwnd,
                    DWMWA_CLOAK,
                    &on as *const BOOL as *const _,
                    mem::size_of::<BOOL>() as u32,
                )
            };
        };
        let made = make(vec![
            plain("plain", "list: plain"),
            Kind {
                then: minimize,
                ..plain("minimized", "list: minimized")
            },
            Kind {
                extended: WS_EX_TOOLWINDOW,
                ..plain("tool", "list: tool")
            },
            Kind {
                owned: true,
                ..plain("owned", "list: owned")
            },
            Kind {
                owned: true,
                style: WS_POPUP,
                ..plain("owned popup", "list: owned popup")
            },
            Kind {
                owned: true,
                extended: WS_EX_APPWINDOW,
                ..plain("owned app", "list: owned app")
            },
            Kind {
                extended: WS_EX_NOACTIVATE,
                ..plain("no activate", "list: no activate")
            },
            Kind {
                extended: WS_EX_NOACTIVATE | WS_EX_APPWINDOW,
                ..plain("no activate app", "list: no activate app")
            },
            Kind {
                then: cloak,
                ..plain("cloaked", "list: cloaked")
            },
            Kind {
                then: exclude,
                ..plain("excluded", "list: excluded")
            },
            plain("untitled", ""),
            plain("blank", " \u{200B} "),
            plain("spoofed", "list: \u{202E}spoof\r\n"),
        ]);
        // SAFETY: a plain query.
        let shell = unsafe { GetShellWindow() };
        let mut programs = HashMap::new();
        let listed: Vec<(&str, Option<TopWindow>)> = made
            .0
            .iter()
            .map(|(name, hwnd)| (*name, listed(*hwnd, 0, shell, &mut programs)))
            .collect();
        for (name, window) in &listed {
            println!("{name}: {window:?}");
        }
        let titles: Vec<(&str, String, bool)> = listed
            .iter()
            .filter_map(|(name, window)| {
                window
                    .as_ref()
                    .map(|window| (*name, window.title.clone(), window.minimized))
            })
            .collect();
        assert_eq!(
            titles,
            [
                ("plain", String::from("list: plain"), false),
                ("minimized", String::from("list: minimized"), true),
                ("owned app", String::from("list: owned app"), false),
                (
                    "no activate app",
                    String::from("list: no activate app"),
                    false
                ),
                ("spoofed", String::from("list: spoof"), false),
            ]
        );
        // This process's own windows are never listed. A count and a time
        // only: other programs' titles never go into test output.
        let start = std::time::Instant::now();
        let all = top_windows().unwrap();
        println!(
            "{} windows listed in {:.1} ms",
            all.len(),
            start.elapsed().as_secs_f64() * 1000.0
        );
        assert!(all.iter().all(|window| {
            !made
                .0
                .iter()
                .any(|(_, hwnd)| hwnd.0 as isize == window.id.hwnd)
        }));
        let program = listed[0]
            .1
            .as_ref()
            .and_then(|window| window.program.clone());
        assert!(program.is_some_and(|program| program.starts_with("capture-")));
    }

    #[test]
    fn a_title_is_cleaned_and_kept_out_of_debug() {
        assert_eq!(clean_title("  \u{2066}a\u{202E}b\u{FEFF}\t "), "ab");
        assert_eq!(clean_title("\u{200D}"), "\u{200D}");
        assert_eq!(clean_title(&"x".repeat(400)).len(), LONGEST_TITLE);
        let window = TopWindow {
            id: WindowId {
                hwnd: 1,
                process: 2,
            },
            title: String::from("secret plans"),
            program: None,
            minimized: false,
        };
        let shown = format!("{window:?}");
        assert!(
            !shown.contains("secret") && shown.contains("title_chars: 12"),
            "{shown}"
        );
    }
}
