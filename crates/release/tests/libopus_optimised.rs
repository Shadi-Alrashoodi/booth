// A target folder that built libopus with no optimisation keeps linking that
// library, and nothing in a cargo build says so. These take the check from each
// script and run it in Windows PowerShell, since neither script can be run short
// of a whole release.

use std::fs;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;

const RELEASE_SCRIPT: &str = include_str!("../../../tools/release.ps1");
const STORE_SCRIPT: &str = include_str!("../../../tools/store.ps1");
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// ec_bsr as cl.exe writes it with no optimisation, as in every release up to 1.2.0.
const UNOPTIMISED: [u8; 26] = [
    0x89, 0x4C, 0x24, 0x08, 0x48, 0x83, 0xEC, 0x18, 0x8B, 0x44, 0x24, 0x20, 0x0F, 0xBD, 0xC0, 0x89,
    0x04, 0x24, 0x8B, 0x04, 0x24, 0x48, 0x83, 0xC4, 0x18, 0xC3,
];

fn check_function(script: &str) -> &str {
    let start = script
        .find("function Assert-OptimisedLibopus")
        .expect("the script has no Assert-OptimisedLibopus");
    // A checkout with core.autocrlf on has CRLF line ends.
    let end = script[start..]
        .find("\n}\n")
        .or_else(|| script[start..].find("\n}\r\n"))
        .expect("Assert-OptimisedLibopus has no end");
    &script[start..start + end + 2]
}

fn run_check(script: &str, name: &str, exe: &[u8]) -> Result<(), String> {
    let path = std::env::temp_dir().join(format!(
        "booth-libopus-optimised-{name}-{}.exe",
        std::process::id()
    ));
    fs::write(&path, exe).unwrap();
    let result = powershell(check_function(script), &path);
    fs::remove_file(&path).unwrap();
    result
}

fn powershell(function: &str, exe: &Path) -> Result<(), String> {
    let script = format!(
        "$ErrorActionPreference = 'Stop'\n{function}\nAssert-OptimisedLibopus $env:BOOTH_TEST_EXE 'C:\\the\\target'"
    );
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .env("BOOTH_TEST_EXE", exe)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .expect("could not run powershell");
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}

fn with_padding(code: &[u8]) -> Vec<u8> {
    let mut exe = vec![0xFFu8, 0x00, 0x80, 0xE9];
    exe.extend_from_slice(code);
    exe.extend_from_slice(&[0xCC, 0x00, 0x90]);
    exe
}

#[test]
fn check_is_found_with_either_line_end() {
    for script in [RELEASE_SCRIPT, STORE_SCRIPT] {
        let lf = script.replace("\r\n", "\n");
        let crlf = lf.replace('\n', "\r\n");
        assert_eq!(
            check_function(&crlf).replace("\r\n", "\n"),
            check_function(&lf)
        );
    }
}

#[test]
fn unoptimised_libopus_stops_both_scripts() {
    for (name, script) in [("release", RELEASE_SCRIPT), ("store", STORE_SCRIPT)] {
        let error = run_check(
            script,
            &format!("{name}-unoptimised"),
            &with_padding(&UNOPTIMISED),
        )
        .expect_err("the check let unoptimised libopus through");
        // PowerShell wraps the message it prints.
        let error: String = error.split_whitespace().collect();
        assert!(
            error.contains("cargoclean-popusic-sys--release--target-dir\"C:\\the\\target\"and"),
            "{name}: {error}"
        );
    }
}

#[test]
fn optimised_code_passes_both_scripts() {
    let mut other = UNOPTIMISED;
    other[10] = 0x25;
    for (name, script) in [("release", RELEASE_SCRIPT), ("store", STORE_SCRIPT)] {
        run_check(script, &format!("{name}-optimised"), &with_padding(&other)).unwrap();
    }
}

#[test]
fn both_scripts_check_the_exe_they_just_built() {
    for (build, check, script) in [
        (
            "Invoke-Tool 'the release build' ",
            "Assert-OptimisedLibopus $booth $target",
            RELEASE_SCRIPT,
        ),
        (
            "Invoke-Tool 'the store build' ",
            "Assert-OptimisedLibopus (Join-Path $target 'release\\booth.exe') $target",
            STORE_SCRIPT,
        ),
    ] {
        let mut lines = script.lines().skip_while(|line| !line.starts_with(build));
        assert!(lines.next().is_some(), "no line starts {build}");
        assert_eq!(lines.next(), Some(check));
    }
}
