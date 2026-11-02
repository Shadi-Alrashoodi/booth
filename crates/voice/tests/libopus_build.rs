// Only a release build compiles libopus's Release configuration, so the library
// these tests link says nothing about the one booth.exe gets. This configures a
// one-file project with the flags the cmake crate gives libopus under the Visual
// Studio generator, and reads what the generated project asks of cl.exe.

use std::env;
use std::fs;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CRATE_FLAGS: &str = " -nologo -MT -Brepro -W0";

struct Folder(PathBuf);

impl Drop for Folder {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn release_libopus_is_compiled_with_optimisation() {
    let dir = env::temp_dir().join(format!("booth-libopus-build-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let dir = Folder(dir);
    fs::write(
        dir.0.join("CMakeLists.txt"),
        "cmake_minimum_required(VERSION 3.16)\nproject(flags C)\nadd_library(flags STATIC flags.c)\n",
    )
    .unwrap();
    fs::write(dir.0.join("flags.c"), "int flags(void) { return 0; }\n").unwrap();

    // Both come from .cargo/config.toml, as they do for the libopus build.
    let cmake = env::var_os("CMAKE").expect("CMAKE is not set");
    let toolchain = env::var("CMAKE_TOOLCHAIN_FILE").expect("CMAKE_TOOLCHAIN_FILE is not set");
    let output = Command::new(cmake)
        .arg(&dir.0)
        .arg("-B")
        .arg(dir.0.join("build"))
        .args(["-A", "x64", "-DCMAKE_BUILD_TYPE=Release"])
        .arg("-DCMAKE_TRY_COMPILE_TARGET_TYPE=STATIC_LIBRARY")
        .arg(format!("-DCMAKE_TOOLCHAIN_FILE={toolchain}"))
        .arg(format!("-DCMAKE_C_FLAGS={CRATE_FLAGS}"))
        .arg(format!("-DCMAKE_C_FLAGS_RELEASE={CRATE_FLAGS}"))
        .env_remove("CMAKE_GENERATOR")
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .expect("could not run cmake");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let project = fs::read_to_string(dir.0.join("build").join("flags.vcxproj")).unwrap();
    let group = "<ItemDefinitionGroup Condition=\"'$(Configuration)|$(Platform)'=='Release|x64'\">";
    let release = &project[project
        .find(group)
        .expect("no Release group in the project")..];
    let setting = release
        .split_once("<Optimization>")
        .and_then(|(_, rest)| rest.split_once("</Optimization>"))
        .expect("no Optimization setting in the Release group")
        .0;
    assert_eq!(setting, "MaxSpeed", "cl.exe would get no /O2 for libopus");
}
