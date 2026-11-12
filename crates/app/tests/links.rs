use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::process::Command;
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION_TYPE, TOKEN_QUERY, TokenElevationType,
    TokenElevationTypeFull,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

// What main counts as an elevated start. booth.exe, started from here,
// has this process's token.
fn elevated() -> bool {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: the pseudo handle for this process needs no closing, and
    // `token` is a live HANDLE for the call to write.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
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
    ok != 0 && kind == TokenElevationTypeFull
}

#[test]
fn malformed_link_runs_exit_without_the_invite_text() {
    // An elevated start is refused before the link is looked at.
    let (exit, said) = if elevated() {
        (
            1,
            "booth: Booth was started as administrator. Start it again the usual way, not with Run as administrator. The firewall step asks for administrator by itself when it needs it.\n",
        )
    } else {
        (2, "")
    };
    let code = OsString::from("booth://join/booth1-private");
    for args in [
        vec![OsString::from("--link")],
        vec![
            "--profile".into(),
            "evil".into(),
            "--link".into(),
            code.clone(),
        ],
        vec![
            "--link".into(),
            code.clone(),
            "--profile".into(),
            "evil".into(),
        ],
        vec!["--link".into(), code.clone(), "--link".into(), code.clone()],
        vec!["--link".into(), OsString::from_wide(&[0xd800])],
        vec!["--link".into(), "x".repeat(2049).into()],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_booth"))
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(exit));
        assert!(output.stdout.is_empty());
        assert_eq!(String::from_utf8(output.stderr).unwrap(), said);
    }
}

#[test]
fn a_link_cannot_add_a_firewall_rule() {
    let output = Command::new(env!("CARGO_BIN_EXE_booth"))
        .args([
            "--link",
            "booth://join/booth1-private",
            "--add-firewall-rule",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let expected = if cfg!(feature = "store") {
        "booth: the Store package brings its own firewall rule, so --add-firewall-rule does nothing in this copy"
    } else {
        "booth: --add-firewall-rule takes nothing else, not even another option"
    };
    assert_eq!(String::from_utf8(output.stderr).unwrap().trim(), expected);
}
