// booth.exe --loopback on the test pattern: capture's pattern, NVENC,
// the video packets, FFmpeg and the viewer, for two seconds, in a window
// shown without taking the focus. Nothing of the screen is captured.

use std::path::Path;
use std::process::Command;

const NVIDIA: u32 = 0x10de;
const FFMPEG: [&str; 2] = ["avcodec-62.dll", "avutil-60.dll"];

// The number right after `label` in the summary line.
fn number_after(line: &str, label: &str) -> u64 {
    let start = line
        .find(label)
        .unwrap_or_else(|| panic!("no {label:?} in {line}"))
        + label.len();
    let digits: String = line[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits
        .parse()
        .unwrap_or_else(|_| panic!("no number after {label:?} in {line}"))
}

#[test]
fn a_short_pattern_loopback_presents_frames_and_exits_cleanly() {
    let exe = Path::new(env!("CARGO_BIN_EXE_booth"));
    let adapters = capture::adapters().unwrap_or_else(|err| panic!("{err}"));
    if !adapters.iter().any(|adapter| adapter.vendor_id == NVIDIA) {
        println!("skipped: no NVIDIA GPU on this PC, and the loopback's default encoder is NVENC");
        return;
    }
    let folder = exe.parent().expect("booth.exe is in a folder");
    if let Some(missing) = FFMPEG.iter().find(|dll| !folder.join(dll).is_file()) {
        println!(
            "skipped: {missing} is not next to {}; build FFmpeg with powershell -ExecutionPolicy Bypass -File tools\\build-ffmpeg.ps1 and build again",
            exe.display()
        );
        return;
    }

    let output = Command::new(exe)
        .args(["--loopback", "--pattern", "1280x720", "--seconds", "2"])
        .output()
        .unwrap_or_else(|err| panic!("could not run {}: {err}", exe.display()));
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("{stderr}");
    assert!(
        output.status.success(),
        "booth.exe ended with {:?}",
        output.status.code()
    );
    let summary = stderr
        .lines()
        .find(|line| line.starts_with("booth: loopback ") && line.contains(" presented "))
        .expect("a summary line");
    // 240 at 120 fps; a quarter of that still shows the path works on a PC
    // busy with something else.
    let presented = number_after(summary, " presented ");
    assert!(presented >= 60, "only {presented} frames presented");
    assert_eq!(number_after(summary, "dropped by the knob "), 0);
    assert_eq!(number_after(summary, "not decoded "), 0);
    // Decode is timed on the GPU, and the times came back.
    assert!(
        summary.contains(" (on the GPU)") && !summary.contains("decode ms none"),
        "{summary}"
    );
}
