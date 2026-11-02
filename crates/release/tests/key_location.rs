// release.ps1 empties folders under dist, so a key in one of them must be
// refused before that happens. These take the script's own lines and run them
// in Windows PowerShell over a throwaway folder with no .git in it, since the
// script has no way to be run short of a whole release.

use std::fs;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const RELEASE_SCRIPT: &str = include_str!("../../../tools/release.ps1");
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

struct Folder(PathBuf);

impl Folder {
    fn new(name: &str) -> Folder {
        let dir =
            std::env::temp_dir().join(format!("booth-key-location-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Folder(dir)
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn script_lines(from: &str, until: &str) -> &'static str {
    let start = RELEASE_SCRIPT
        .find(from)
        .unwrap_or_else(|| panic!("release.ps1 has no {from}"));
    let rest = &RELEASE_SCRIPT[start..];
    let end = rest
        .find(until)
        .unwrap_or_else(|| panic!("release.ps1 has no {until} after {from}"));
    &rest[..end]
}

// In a block, since a preference set at the top of -Command is global and would
// reach a module function like Get-FileHash, where the script's own does not.
fn powershell(script: &str, root: &Path, key: &Path) -> String {
    let script = format!("& {{\n$ErrorActionPreference = 'Stop'\n{script}\n}}");
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .env("BOOTH_TEST_ROOT", root)
        .env("BOOTH_TEST_KEY", key)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .expect("could not run powershell");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}{stderr}");
    stdout
}

fn place_key(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, "the key").unwrap();
}

fn location_check(root: &Path, key: &Path) -> String {
    let check = script_lines(
        "$SecretKey = (Resolve-Path -LiteralPath $SecretKey).Path",
        "# The key's password from Windows Credential Manager",
    );
    powershell(
        &format!(
            "$root = $env:BOOTH_TEST_ROOT
            $SecretKey = $env:BOOTH_TEST_KEY
            try {{
                {check}
                'passed'
            }} catch {{
                \"refused: $_\"
            }}"
        ),
        root,
        key,
    )
}

#[test]
fn refuses_a_key_inside_the_folder_with_no_git_there() {
    let folder = Folder::new("inside");
    let root = folder.0.join("booth");
    let key = root.join(r"dist\1.2.3\test.key");
    place_key(&key);
    let said = location_check(&root, &key);
    assert!(said.starts_with("refused: "), "{said}");
    assert!(said.contains("Nothing was deleted"), "{said}");

    let other_case = PathBuf::from(key.to_string_lossy().to_uppercase());
    let said = location_check(&root, &other_case);
    assert!(said.starts_with("refused: "), "{said}");

    let beside = folder.0.join(r"booth-keys\test.key");
    place_key(&beside);
    let said = location_check(&root, &beside);
    assert_eq!(said.trim(), "passed");
}

// With the tools stubbed: check-key refuses, or lets it go on.
fn empty_dist_after_check(root: &Path, key: &Path, check_key: &str) -> String {
    let clear_folder = script_lines("function Clear-Folder", "# System.IO.Compression");
    let section = script_lines("$dist = Join-Path $root", "# --- Third-party files");
    powershell(
        &format!(
            "$root = $env:BOOTH_TEST_ROOT
            $SecretKey = $env:BOOTH_TEST_KEY
            $target = Join-Path $root 'target'
            $version = '1.2.3'
            {clear_folder}
            function Invoke-Tool {{ }}
            function Invoke-WithKey {{ {check_key} }}
            try {{
                {section}
                'went on'
            }} catch {{
                \"stopped: $_\"
            }}"
        ),
        root,
        key,
    )
}

#[test]
fn a_refused_key_stays_where_it_is() {
    for place in [r"dist\1.2.3\test.key", r"dist\staging\test.key"] {
        let folder = Folder::new("refused");
        let key = folder.0.join(place);
        place_key(&key);
        let said = empty_dist_after_check(&folder.0, &key, "throw 'inside the git repository'");
        assert!(
            said.contains("stopped: inside the git repository"),
            "{said}"
        );
        assert!(key.is_file(), "{place} was deleted: {said}");
    }
}

#[test]
fn an_accepted_key_lets_the_folders_be_emptied() {
    let folder = Folder::new("accepted");
    let dist = folder.0.join(r"dist\1.2.3");
    let staging = folder.0.join(r"dist\staging");
    place_key(&dist.join("old.zip"));
    place_key(&staging.join("old.txt"));
    // As long as what is in the folders, and other bytes.
    let key = folder.0.join(r"keys\test.key");
    place_key(&key);
    fs::write(&key, "the kez").unwrap();
    let said = empty_dist_after_check(&folder.0, &key, "");
    assert!(said.contains("went on"), "{said}");
    assert_eq!(fs::read_dir(&dist).unwrap().count(), 0, "{said}");
    assert!(!staging.exists(), "{said}");
    assert!(key.is_file(), "{said}");
}

fn junction(link: &Path, target: &Path) {
    let made = Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .args([link, target])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .unwrap();
    assert!(made.status.success(), "{made:?}");
}

// Passes the check on the key's path and, with no .git to find, check-key.
fn stays_though_both_checks_pass(root: &Path, key: &Path, kept: &Path) {
    assert_eq!(location_check(root, key).trim(), "passed");
    let said = empty_dist_after_check(root, key, "");
    assert!(said.contains("stopped: "), "{said}");
    assert!(said.contains("same bytes as the release key"), "{said}");
    assert!(said.contains("Nothing was deleted"), "{said}");
    assert!(kept.is_file(), "{} was deleted: {said}", kept.display());
}

#[test]
fn a_key_reached_through_a_junction_into_dist_stays() {
    let folder = Folder::new("junction-into-dist");
    let root = folder.0.join("booth");
    let key = root.join(r"dist\1.2.3\test.key");
    place_key(&key);
    let link = folder.0.join("keys");
    junction(&link, &root.join(r"dist\1.2.3"));
    stays_though_both_checks_pass(&root, &link.join("test.key"), &key);
}

#[test]
fn a_key_in_the_folder_dist_is_a_junction_to_stays() {
    let folder = Folder::new("dist-is-junction");
    let root = folder.0.join("booth");
    fs::create_dir_all(&root).unwrap();
    let moved = folder.0.join("bigger-drive");
    let key = moved.join(r"1.2.3\test.key");
    place_key(&key);
    junction(&root.join("dist"), &moved);
    stays_though_both_checks_pass(&root, &key, &key);
}

#[test]
fn a_copy_of_the_key_in_staging_stays() {
    let folder = Folder::new("copy");
    let root = folder.0.join("booth");
    let copy = root.join(r"dist\staging\nested\copy.key");
    place_key(&copy);
    let key = folder.0.join(r"keys\test.key");
    place_key(&key);
    stays_though_both_checks_pass(&root, &key, &copy);
}

// Another process has these open so that they cannot be read and can still be
// deleted, so what stops the release is the hash and not the delete failing.
#[test]
fn a_file_that_cannot_be_hashed_stops_the_emptying() {
    const FILE_SHARE_DELETE: u32 = 4;
    let folder = Folder::new("cannot-hash");
    let in_dist = folder.0.join(r"dist\1.2.3\old.zip");
    let in_staging = folder.0.join(r"dist\staging\old.txt");
    let key = folder.0.join(r"keys\test.key");
    place_key(&in_dist);
    place_key(&in_staging);
    place_key(&key);
    fs::write(&key, "the kez").unwrap();
    for held in [&in_dist, &in_staging, &key] {
        let _open = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_DELETE)
            .open(held)
            .unwrap();
        let said = empty_dist_after_check(&folder.0, &key, "");
        assert!(said.contains("stopped: "), "{}: {said}", held.display());
        assert!(in_dist.is_file(), "{}: {said}", held.display());
        assert!(in_staging.is_file(), "{}: {said}", held.display());
    }
}
