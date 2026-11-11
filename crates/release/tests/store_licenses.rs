// A Store install's FFmpeg DLLs cannot be replaced, so the Store package's
// license texts point to the zip, and only store.ps1 asks for them.

use std::os::windows::process::CommandExt;
use std::process::Command;

const RELEASE_SCRIPT: &str = include_str!("../../../tools/release.ps1");
const STORE_SCRIPT: &str = include_str!("../../../tools/store.ps1");
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[test]
fn scripts_pick_their_layout() {
    for (script, call) in [
        (RELEASE_SCRIPT, "@('licenses', $root, $files)"),
        (STORE_SCRIPT, "@('licenses', '--store', $root, $layout)"),
    ] {
        let line = script
            .lines()
            .find(|line| line.starts_with("Invoke-Tool 'collecting the third-party licenses' "))
            .expect("no line collects the licenses");
        assert!(line.trim_end().ends_with(call), "{line}");
    }
}

// It gets past the usage and stops at the folder that is not there.
#[test]
fn licenses_takes_store_flag() {
    let missing = std::env::temp_dir().join(format!("booth-no-repository-{}", std::process::id()));
    let output = Command::new(env!("CARGO_BIN_EXE_release"))
        .arg("licenses")
        .arg("--store")
        .arg(&missing)
        .arg(missing.join("out"))
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.starts_with("release: could not run") && !stderr.contains("usage:"),
        "{stderr}"
    );
}

#[test]
fn store_needs_both_folders() {
    let output = Command::new(env!("CARGO_BIN_EXE_release"))
        .args(["licenses", "--store", "repository"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.starts_with("usage:"), "{stderr}");
}
