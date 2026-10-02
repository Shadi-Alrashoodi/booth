use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;

fn main() {
    let manifest =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let ffmpeg = manifest
        .join("..")
        .join("..")
        .join("third_party")
        .join("ffmpeg");
    let include = ffmpeg.join("include");
    let bin = ffmpeg.join("bin");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/fields.c");
    println!("cargo:rerun-if-changed={}", include.display());
    println!("cargo:rerun-if-changed={}", bin.display());

    if !include.join("libavcodec").join("avcodec.h").is_file() {
        eprintln!(
            "third_party/ffmpeg is missing: build it once with powershell -ExecutionPolicy Bypass -File tools\\build-ffmpeg.ps1"
        );
        process::exit(1);
    }

    // cc follows the crt-static target feature from .cargo/config.toml, so
    // this builds against the static C runtime like the rest of booth.exe.
    // No FFmpeg library is linked: the file only touches struct fields.
    cc::Build::new()
        .file("src/fields.c")
        .include(&include)
        .std("c11")
        .warnings(true)
        .compile("ffmpeg_fields");

    // The decoder loads FFmpeg only from the folder of the running exe, so
    // tests, examples and cargo run find it only if it is copied there.
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    // OUT_DIR is <profile>/build/decode-<hash>/out.
    let Some(profile) = out.ancestors().nth(3) else {
        eprintln!(
            "could not find the build profile folder above {}",
            out.display()
        );
        process::exit(1);
    };
    let dlls = match dlls_in(&bin) {
        Ok(dlls) => dlls,
        Err(err) => {
            eprintln!("could not list {}: {err}", bin.display());
            process::exit(1);
        }
    };
    for folder in [
        profile.to_path_buf(),
        profile.join("deps"),
        profile.join("examples"),
    ] {
        if let Err(err) = fs::create_dir_all(&folder) {
            eprintln!("could not create {}: {err}", folder.display());
            process::exit(1);
        }
        for dll in &dlls {
            let Some(name) = dll.file_name() else {
                continue;
            };
            let to = folder.join(name);
            if let Err(err) = copy_if_changed(dll, &to) {
                eprintln!(
                    "could not copy {} to {}: {err}",
                    dll.display(),
                    to.display()
                );
                process::exit(1);
            }
        }
    }
}

fn dlls_in(folder: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut dlls = Vec::new();
    for entry in fs::read_dir(folder)? {
        let path = entry?.path();
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("dll"))
        {
            dlls.push(path);
        }
    }
    Ok(dlls)
}

// Windows refuses to overwrite a DLL that a running booth.exe or test has
// loaded, so a copy that is current is left alone. fs::copy keeps the
// modification time on Windows, so size and time match once a copy is
// current.
fn copy_if_changed(from: &Path, to: &Path) -> std::io::Result<()> {
    let source = fs::metadata(from)?;
    if let Ok(target) = fs::metadata(to)
        && target.len() == source.len()
        && target.modified()? == source.modified()?
    {
        return Ok(());
    }
    fs::copy(from, to)?;
    Ok(())
}
