use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=booth.manifest");
    println!("cargo:rerun-if-changed=assets/booth.ico");
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    // Windows looks for an exe's imports in the exe's own folder before
    // System32, unless they are on its KnownDLLs list, and several of
    // booth.exe's are not: dwmapi, uxtheme, dxgi, iphlpapi and others. A
    // friend's Downloads folder can hold anything, and the firewall step runs
    // this same exe as administrator, so every import comes from System32.
    // 0x800 is LOAD_LIBRARY_SEARCH_SYSTEM32.
    println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");

    // Inside the exe rather than beside it, so there is no second file to
    // lose. The linker's own trustInfo is left out, since booth.manifest has
    // one and is meant to be the whole manifest.
    let dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let manifest = dir.join("booth.manifest");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
        manifest.display()
    );
    println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:NO");

    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let res = resources(&out, &dir.join("assets"));
    println!("cargo:rustc-link-arg-bins={}", res.display());
}

// The icon, and the company, name and version Windows shows under
// Properties, Details, and the name Task Manager lists the process under. The
// version comes from Cargo.toml so it is only ever written in one place.
fn resources(out: &Path, assets: &Path) -> PathBuf {
    let version = env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let part = |name: &str| {
        env::var(format!("CARGO_PKG_VERSION_{name}")).expect("cargo sets the version parts")
    };
    let numbers = format!("{},{},{},0", part("MAJOR"), part("MINOR"), part("PATCH"));
    // Explorer and every shortcut to booth.exe show the first icon in the
    // exe, and this is the only one. Each size in booth.ico is drawn on its
    // own grid rather than shrunk from a big one.
    //
    // 0x40004 and 0x1 are VOS_NT_WINDOWS32 and VFT_APP. winver.h has the
    // names, but rc.exe is not given the SDK's include folders to find it.
    let script = format!(
        r#"1 ICON "booth.ico"

1 VERSIONINFO
FILEVERSION {numbers}
PRODUCTVERSION {numbers}
FILEOS 0x40004
FILETYPE 0x1
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904B0"
        BEGIN
            VALUE "CompanyName", "Shadi Alrashoodi"
            VALUE "FileDescription", "Booth"
            VALUE "FileVersion", "{version}"
            VALUE "InternalName", "booth"
            VALUE "LegalCopyright", "Copyright (c) 2026 Shadi Alrashoodi"
            VALUE "OriginalFilename", "booth.exe"
            VALUE "ProductName", "Booth"
            VALUE "ProductVersion", "{version}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#
    );
    let source = out.join("booth.rc");
    if let Err(err) = fs::write(&source, script) {
        eprintln!("could not write {}: {err}", source.display());
        process::exit(1);
    }

    let Some(rc) = find_rc() else {
        eprintln!(
            "could not find rc.exe, the Windows SDK's resource compiler, under Windows Kits\\10\\bin or on PATH. Add a Windows 11 SDK to the Visual Studio 2022 Build Tools in the Visual Studio Installer."
        );
        process::exit(1);
    };
    // A compiled resource file goes to link.exe like an object file. The
    // icon is found by name through /i rather than written into the script
    // as a path: rc.exe reads the script in the ANSI code page, so a checkout
    // in a folder whose name is not plain ASCII would not find it.
    let res = out.join("booth.res");
    match Command::new(&rc)
        .arg("/nologo")
        .arg("/i")
        .arg(assets)
        .arg("/fo")
        .arg(&res)
        .arg(&source)
        .status()
    {
        Ok(status) if status.success() => res,
        Ok(status) => {
            eprintln!("{} failed on {}: {status}", rc.display(), source.display());
            process::exit(1);
        }
        Err(err) => {
            eprintln!("could not run {}: {err}", rc.display());
            process::exit(1);
        }
    }
}

// The newest SDK in the default Windows Kits folder, which is where the
// Build Tools put it. A Visual Studio prompt also has rc.exe on PATH, for an
// SDK installed somewhere else.
fn find_rc() -> Option<PathBuf> {
    let mut newest: Option<(Vec<u32>, PathBuf)> = None;
    if let Some(programs) = env::var_os("ProgramFiles(x86)") {
        let bin = Path::new(&programs).join(r"Windows Kits\10\bin");
        for entry in fs::read_dir(bin).into_iter().flatten().flatten() {
            let name = entry.file_name();
            let Some(version) = name.to_str().and_then(|name| {
                name.split('.')
                    .map(|number| number.parse().ok())
                    .collect::<Option<Vec<u32>>>()
            }) else {
                continue;
            };
            let rc = entry.path().join(r"x64\rc.exe");
            if rc.is_file() && newest.as_ref().is_none_or(|(best, _)| version > *best) {
                newest = Some((version, rc));
            }
        }
    }
    newest.map(|(_, rc)| rc).or_else(|| {
        let path = env::var_os("PATH")?;
        env::split_paths(&path)
            .map(|folder| folder.join("rc.exe"))
            .find(|rc| rc.is_file())
    })
}
