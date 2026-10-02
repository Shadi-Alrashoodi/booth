// The Windows part: the elevation check. Nothing here makes a window,
// registers for input or sends a key. The hidden window's thread is tested
// in src/win/tests.rs, where a test build keeps background input from it.

use input::{Elevation, process_elevation, this_process_elevated};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, HANDLE};
use windows_sys::Win32::Security::TOKEN_QUERY;
use windows_sys::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTS_CURRENT_SERVER_HANDLE, WTS_PROCESS_INFOW, WTSEnumerateProcessesW,
    WTSFreeMemory,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

#[test]
fn this_process_is_not_elevated() {
    if this_process_elevated() {
        eprintln!("skipped: the tests run as administrator, so this process is elevated");
        return;
    }
    // SAFETY: takes no arguments.
    let pid = unsafe { GetCurrentProcessId() };
    assert_eq!(process_elevation(pid).unwrap(), Elevation::Normal);
}

// Winlogon runs as SYSTEM in every signed-in session, which is more than any
// administrator window has, and is always there. Nothing is started for this.
// This process may not even open it.
#[test]
fn a_process_that_runs_with_more_rights_counts_as_elevated() {
    if this_process_elevated() {
        eprintln!("skipped: the tests run as administrator, so nothing is above them");
        return;
    }
    let Some(pid) = in_this_session()
        .into_iter()
        .find(|(_, name)| name.eq_ignore_ascii_case("winlogon.exe"))
        .map(|(pid, _)| pid)
    else {
        eprintln!("skipped: no winlogon.exe in this session to check against");
        return;
    };
    let elevation = process_elevation(pid).unwrap();
    assert_ne!(elevation, Elevation::Normal, "winlogon.exe, process {pid}");
}

// The other way an administrator window looks from here: this process may
// open it, but not its token. Some process in this session usually runs that
// way (a vendor's service helper, say); nothing is started for this.
#[test]
fn a_process_whose_token_is_refused_counts_as_elevated() {
    if this_process_elevated() {
        eprintln!("skipped: the tests run as administrator, so nothing is above them");
        return;
    }
    for (pid, name) in in_this_session() {
        if !token_refused(pid) {
            continue;
        }
        match process_elevation(pid) {
            Ok(elevation) => {
                assert_eq!(elevation, Elevation::Unreadable, "{name}, process {pid}");
                return;
            }
            // It ended between the two looks.
            Err(_) if !token_refused(pid) => continue,
            Err(err) => panic!("{name}, process {pid}: {err}"),
        }
    }
    eprintln!("skipped: no process in this session opens but refuses its token to this one");
}

#[test]
fn a_process_that_is_gone_is_an_error_not_a_pause() {
    // Process ids are multiples of 4, so this one is never in use.
    let err = process_elevation(3).unwrap_err();
    assert!(
        err.to_string().starts_with("could not open process 3"),
        "{err}"
    );
}

// Opened for the least there is, and its token refused as access denied.
fn token_refused(pid: u32) -> bool {
    // SAFETY: plain values; a null result is checked.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return false;
    }
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: `process` is open with the access OpenProcessToken needs, and
    // `token` is a live HANDLE for the call to fill.
    let opened = unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } != 0;
    let denied = !opened
        && std::io::Error::last_os_error().raw_os_error() == Some(ERROR_ACCESS_DENIED as i32);
    // SAFETY: each is closed once, and not used after.
    unsafe {
        if opened {
            CloseHandle(token);
        }
        CloseHandle(process);
    }
    denied
}

// The session list names every process with its session, without opening
// any of them.
fn in_this_session() -> Vec<(u32, String)> {
    let mut ours = 0;
    // SAFETY: fills one live u32.
    if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut ours) } == 0 {
        return Vec::new();
    }
    let mut list: *mut WTS_PROCESS_INFOW = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: this PC's own list, version 1 as the call requires.
    let listed =
        unsafe { WTSEnumerateProcessesW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut list, &mut count) };
    if listed == 0 || list.is_null() {
        return Vec::new();
    }
    // SAFETY: on success `list` holds `count` entries until WTSFreeMemory.
    let processes = unsafe { std::slice::from_raw_parts(list, count as usize) };
    let found = processes
        .iter()
        .filter(|process| process.SessionId == ours && !process.pProcessName.is_null())
        .map(|process| (process.ProcessId, name(process.pProcessName)))
        .collect();
    // SAFETY: freed once, after the last read of it.
    unsafe { WTSFreeMemory(list.cast()) };
    found
}

fn name(text: *const u16) -> String {
    let mut len = 0;
    // SAFETY: a zero-terminated name that lives as long as the list, read up
    // to its terminator.
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: the `len` units just read.
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) })
}
