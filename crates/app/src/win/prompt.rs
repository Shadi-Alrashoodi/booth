// The administrator prompt for the firewall step.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, HANDLE, HWND, WAIT_OBJECT_0};
use windows_sys::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
use windows_sys::Win32::UI::Shell::{
    SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    ShellExecuteExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

use super::Elevated;

// Starts this exe again through the administrator prompt, with one argument
// and no window, and waits for it to end. That takes as long as the user
// looks at the prompt, so it runs on a thread of its own. `owner` is the
// panel's window, so the prompt belongs to it.
pub fn run_elevated(exe: &Path, argument: &str, owner: Option<isize>) -> Elevated {
    // ShellExecuteEx can hand the work to shell extensions, which need COM,
    // but only for the call itself. The wait after it pumps no messages,
    // which a thread in a COM apartment would owe anyone calling in.
    // SAFETY: no reserved pointer; a success is paired with the
    // CoUninitialize below, on this same thread.
    let com = unsafe {
        CoInitializeEx(
            ptr::null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        )
    } >= 0;
    let started = start_elevated(exe, argument, owner);
    if com {
        // SAFETY: pairs the CoInitializeEx above. With NOASYNC the shell is
        // done with COM once the call has returned.
        unsafe { CoUninitialize() };
    }
    match started {
        Ok(process) => wait_for_exit(process),
        Err(err) if err.raw_os_error() == Some(ERROR_CANCELLED as i32) => Elevated::Cancelled,
        Err(err) => Elevated::Failed(err),
    }
}

fn start_elevated(exe: &Path, argument: &str, owner: Option<isize>) -> io::Result<HANDLE> {
    let verb = wide("runas");
    let file = wide(exe);
    let parameters = wide(argument);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        // NO_UI keeps the shell's own error box away, so the one message
        // the user sees is Booth's; the consent prompt is not an error box
        // and still shows. NOASYNC because this thread has no message loop.
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_FLAG_NO_UI | SEE_MASK_NOASYNC,
        hwnd: owner.map_or(ptr::null_mut(), |hwnd| hwnd as HWND),
        lpVerb: verb.as_ptr(),
        lpFile: file.as_ptr(),
        lpParameters: parameters.as_ptr(),
        nShow: SW_HIDE,
        ..SHELLEXECUTEINFOW::default()
    };
    // SAFETY: `info` carries its own size, every pointer in it is null or a
    // zero-terminated string that outlives the call, and the call writes
    // only into `info`.
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.hProcess.is_null() {
        return Err(io::Error::other(
            "Windows accepted the prompt but gave back no process to wait for",
        ));
    }
    Ok(info.hProcess)
}

// Takes the process handle and closes it.
fn wait_for_exit(process: HANDLE) -> Elevated {
    let mut code = 0u32;
    // SAFETY: `process` is the live handle ShellExecuteExW returned, owned
    // here, and `code` is a live u32 for the exit code.
    let exited = unsafe {
        WaitForSingleObject(process, INFINITE) == WAIT_OBJECT_0
            && GetExitCodeProcess(process, &mut code) != 0
    };
    let failed = (!exited).then(io::Error::last_os_error);
    // SAFETY: closed once, and not used after this.
    unsafe { CloseHandle(process) };
    match failed {
        None => Elevated::Exited(code),
        Some(err) => Elevated::Failed(err),
    }
}

fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().chain([0]).collect()
}
