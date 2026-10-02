//! The release tool: makes the key that signs Booth's releases, signs them,
//! and writes the third-party licenses that go in the zip. tools\release.ps1
//! runs it; nothing here touches the network.

mod expression;
mod ffmpeg;
mod json;
mod licenses;
mod signing;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "usage:
  release keygen <secret key file>
      makes a password-protected key pair, writes the secret key to the file,
      which must be outside any git repository, and prints the public key
  release check-key [--rehearsal] <secret key file> <public key>
      asks for the key's password and refuses the key unless its public half
      is <public key>, the app's RELEASE_KEY; with --rehearsal it only warns
  release sign [--rehearsal] <secret key file> <public key> <file>...
      checks the key as check-key does, then writes <file>.minisig for each
      file; the key file must be outside any git repository
  release licenses <repository folder> <output folder>
      writes THIRD-PARTY-LICENSES.txt and the ffmpeg folder into the output folder";

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let command = args.first().and_then(|a| a.to_str());
    let rehearsal = args.get(1).is_some_and(|a| a == "--rehearsal");
    let rest = args.get(1 + usize::from(rehearsal)..).unwrap_or_default();
    let result = match (command, rehearsal, rest.len()) {
        (Some("keygen"), false, 1) => keygen(Path::new(&rest[0])),
        (Some("check-key"), _, 2) => {
            open_checked_key(Path::new(&rest[0]), &rest[1], rehearsal).map(drop)
        }
        (Some("sign"), _, 3..) => sign(Path::new(&rest[0]), &rest[1], &rest[2..], rehearsal),
        (Some("licenses"), false, 2) => collect_licenses(Path::new(&rest[0]), Path::new(&rest[1])),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("release: {err}");
            ExitCode::FAILURE
        }
    }
}

fn keygen(path: &Path) -> Result<(), String> {
    let public = signing::keygen(path)?;
    println!("public key: {}", public.base64);
    println!("key id: {}", public.id);
    println!(
        "The secret key is in {}. The public key goes in {}; until it is there, the release tool signs only rehearsals with this key. Keep the key file and its password apart, and the file off this PC except while signing.",
        path.display(),
        signing::APP_KEY
    );
    Ok(())
}

// release.ps1 runs this as check-key before anything but this tool is
// built, so a wrong key stops it before the long build. sign runs it as
// well, for when the tool is run by hand.
fn open_checked_key(
    key_path: &Path,
    app_key: &OsStr,
    rehearsal: bool,
) -> Result<(minisign::SecretKey, String), String> {
    let key = signing::open_secret_key(key_path)?;
    if !key.has_password {
        eprintln!(
            "warning: {} has no password, so anyone who copies it can sign as Booth; use a key like this only for a test release",
            key_path.display()
        );
    }
    let public = minisign::PublicKey::from_secret_key(&key.secret).map_err(|err| {
        format!(
            "could not read the public half of {}: {err}",
            key_path.display()
        )
    })?;
    let id = signing::public_line(&public).id;
    match signing::against_app_key(&public, key_path, &app_key.to_string_lossy(), rehearsal)? {
        Some(warning) => eprintln!("warning: {warning}"),
        None => println!(
            "{} is the key {id}, the one in RELEASE_KEY",
            key_path.display()
        ),
    }
    Ok((key.secret, id))
}

fn sign(
    key_path: &Path,
    app_key: &OsStr,
    files: &[OsString],
    rehearsal: bool,
) -> Result<(), String> {
    let files: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
    for file in &files {
        if !file.is_file() {
            return Err(format!(
                "{} is not a file; nothing was signed",
                file.display()
            ));
        }
    }
    let (secret, id) = open_checked_key(key_path, app_key, rehearsal)?;
    for written in signing::sign_files(&secret, &files)? {
        println!("signed {} with key {id}", written.display());
    }
    Ok(())
}

fn collect_licenses(root: &Path, out: &Path) -> Result<(), String> {
    let metadata = licenses::cargo_metadata(root)?;
    let tree = licenses::cargo_tree(root)?;
    let (collected, ffmpeg) = licenses::collect(root, &metadata, &tree)?;
    fs::create_dir_all(out).map_err(|err| format!("could not create {}: {err}", out.display()))?;
    let file = out.join("THIRD-PARTY-LICENSES.txt");
    licenses::write_crlf(&file, &collected.file)?;
    ffmpeg.write_folder(out)?;
    println!("wrote {}: {} crates", file.display(), collected.crate_count);
    println!(
        "wrote {}: {} under the LGPL {} or later",
        out.join("ffmpeg").display(),
        ffmpeg.dlls.join(", "),
        ffmpeg.license.name()
    );
    for line in &collected.manual {
        println!("check by hand: {line}");
    }
    Ok(())
}
