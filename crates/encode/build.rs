use std::env;
use std::path::PathBuf;
use std::process;

fn main() {
    let manifest =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let headers = manifest
        .join("..")
        .join("..")
        .join("third_party")
        .join("nv-codec-headers");
    let header = headers.join("nvEncodeAPI.h");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/nvenc/layout.c");
    println!("cargo:rerun-if-changed={}", header.display());

    if !header.is_file() {
        eprintln!(
            "third_party/nv-codec-headers/nvEncodeAPI.h is missing: run powershell -ExecutionPolicy Bypass -File tools\\fetch-third-party.ps1 once"
        );
        process::exit(1);
    }

    // Only the layout test calls into this object, so nothing from it ends up
    // in booth.exe. cc follows the crt-static target feature from
    // .cargo/config.toml and builds it against the static C runtime.
    cc::Build::new()
        .file("src/nvenc/layout.c")
        .include(&headers)
        .warnings(true)
        .compile("nvenc_layout");
}
