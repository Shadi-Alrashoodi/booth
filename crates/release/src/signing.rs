// The release key. keygen makes a password-protected minisign key pair and
// writes only the secret half, to a file that must be outside any git
// repository; the public half is printed for the update check's constant.
// sign refuses a key inside a repository too, asks for the password (three
// tries) or reads it once from a pipe, refuses a key whose public half is
// not that constant, and signs each file into file.minisig. The secret key
// and its password never reach stdout, stderr or a log.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use minisign::{ErrorKind, KeyPair, PublicKey, SecretKey, SecretKeyBox};

const KEY_COMMENT: &str = "Booth release secret key";
const SIGNATURE_COMMENT: &str = "signature from the Booth release key";
// Where every copy of Booth gets the public key it checks a release
// against. tools\release.ps1 reads it from there and passes it in.
pub const APP_KEY: &str = r"RELEASE_KEY in crates\app\src\update\mod.rs";
// A minisign secret key file is about 300 bytes.
const MOST_KEY_BYTES: u64 = 1024;
const PASSWORD_TRIES: u32 = 3;
// minisign takes no longer password at the console either.
const MOST_PASSWORD_BYTES: usize = 1024;

pub enum Password<'a> {
    // minisign asks on the console, PASSWORD_TRIES times at most.
    Console,
    // Read once, to the end. tools\release.ps1 pipes in the password it
    // finds in Windows Credential Manager.
    Piped(&'a mut dyn Read),
}

pub struct ReleaseKey {
    pub secret: SecretKey,
    pub has_password: bool,
}

pub struct PublicKeyLine {
    pub base64: String,
    pub id: String,
}

pub fn keygen(path: &Path) -> Result<PublicKeyLine, String> {
    check_secret_path(path)?;
    // minisign asks for the password twice and refuses a mismatch.
    let pair = KeyPair::generate_encrypted_keypair(None)
        .map_err(|err| format!("could not make the key: {err}"))?;
    let secret = pair
        .sk
        .to_box(Some(KEY_COMMENT))
        .map_err(|err| format!("could not encode the key: {err}"))?;
    // minisign accepts an empty password, which leaves the key readable by
    // anyone who copies the file.
    if SecretKey::from_box(SecretKeyBox::from(secret.to_string()), Some(String::new())).is_ok() {
        return Err(
            "an empty password leaves the key unprotected; nothing was written, run keygen again"
                .to_string(),
        );
    }
    write_secret(path, &secret)?;
    Ok(public_line(&pair.pk))
}

pub fn check_secret_path(path: &Path) -> Result<(), String> {
    if path.exists() {
        return Err(format!(
            "{} already exists; keygen never overwrites a key",
            path.display()
        ));
    }
    let absolute = std::path::absolute(path)
        .map_err(|err| format!("could not resolve {}: {err}", path.display()))?;
    let parent = absolute
        .parent()
        .ok_or_else(|| format!("{} has no folder", path.display()))?;
    if !parent.is_dir() {
        return Err(format!(
            "{} does not exist; make the folder first",
            parent.display()
        ));
    }
    if let Some(repository) = repository_around(parent) {
        return Err(format!(
            "{} is inside the git repository at {}; keep the release key outside it, on a drive that is only plugged in to sign",
            path.display(),
            repository.display()
        ));
    }
    Ok(())
}

// Through canonicalize, so a subst drive or a junction into the repository
// is still seen as inside it.
fn repository_around(folder: &Path) -> Option<PathBuf> {
    let real = fs::canonicalize(folder).unwrap_or_else(|_| folder.to_path_buf());
    let found = real
        .ancestors()
        .find(|dir| dir.join(".git").exists())?
        .to_string_lossy()
        .to_string();
    Some(PathBuf::from(found.strip_prefix(r"\\?\").unwrap_or(&found)))
}

fn write_secret(path: &Path, secret: &SecretKeyBox) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|err| format!("could not create {}: {err}", path.display()))?;
    file.write_all(&secret.to_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|err| format!("could not write {}: {err}", path.display()))
}

pub fn public_line(pk: &PublicKey) -> PublicKeyLine {
    let id = pk
        .keynum()
        .iter()
        .rev()
        .map(|b| format!("{b:02X}"))
        .collect();
    PublicKeyLine {
        base64: pk.to_base64(),
        id,
    }
}

// Every installed copy refuses a release that RELEASE_KEY does not verify,
// so one signed with any other key would reach nobody. A rehearsal is never
// published, so there it is a warning and a throwaway key still works.
// Compared as the text the app decodes: minisign's own == looks only at the
// 32 key bytes, and the app also refuses a signature whose key id differs.
pub fn against_app_key(
    found: &PublicKey,
    key_path: &Path,
    app_key: &str,
    rehearsal: bool,
) -> Result<Option<String>, String> {
    let found = public_line(found);
    if found.base64 == app_key {
        return Ok(None);
    }
    let app = PublicKey::from_base64(app_key)
        .ok()
        .map(|pk| public_line(&pk))
        .filter(|app| app.base64 == app_key);
    let (problem, fix) = match app {
        Some(app) => (
            format!(
                "{} is the key {} ({}), but Booth checks releases against {APP_KEY}, the key {} ({}), so every installed copy would refuse what it signs",
                key_path.display(),
                found.id,
                found.base64,
                app.id,
                app.base64
            ),
            "sign with the key whose public half is in RELEASE_KEY",
        ),
        None => (
            format!(
                "{APP_KEY} is \"{app_key}\", not a public key yet, so no copy of Booth could check a release"
            ),
            "make the release key once with: cargo run --release --locked -p release -- keygen <path outside this repository>, put the public key it prints in RELEASE_KEY, and run the release again",
        ),
    };
    if rehearsal {
        Ok(Some(format!(
            "{problem}; a rehearsal goes on anyway, but never publish what it makes"
        )))
    } else {
        Err(format!("{problem}; {fix}"))
    }
}

pub fn open_secret_key(path: &Path, password: Password) -> Result<ReleaseKey, String> {
    let absolute = std::path::absolute(path)
        .map_err(|err| format!("could not resolve {}: {err}", path.display()))?;
    if let Some(repository) = absolute.parent().and_then(repository_around) {
        return Err(format!(
            "{} is inside the git repository at {}; move the release key out of it, check that git never took it in, and sign again",
            path.display(),
            repository.display()
        ));
    }
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(MOST_KEY_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|err| format!("could not read the key {}: {err}", path.display()))?;
    if bytes.len() as u64 > MOST_KEY_BYTES {
        return Err(format!(
            "{} is larger than a minisign secret key ever is, so it is not one",
            path.display()
        ));
    }
    let not_a_key = || format!("{} is not a minisign secret key", path.display());
    let text = String::from_utf8(bytes).map_err(|_| not_a_key())?;
    let has_password = text
        .lines()
        .nth(1)
        .and_then(kdf_is_scrypt)
        .ok_or_else(not_a_key)?;
    let could_not_open =
        |err: minisign::PError| format!("could not open the key {}: {err}", path.display());
    let secret = if !has_password {
        SecretKey::from_unencrypted_box(SecretKeyBox::from(text)).map_err(could_not_open)
    } else {
        match password {
            // minisign asks for the password on the console each time.
            Password::Console => with_tries(PASSWORD_TRIES, || {
                SecretKey::from_box(SecretKeyBox::from(text.clone()), None)
            })
            .map_err(could_not_open),
            // A wrong password from a pipe would only come in wrong again.
            Password::Piped(from) => {
                let password = read_password(from)?;
                SecretKey::from_box(SecretKeyBox::from(text), Some(password)).map_err(|err| {
                    match err.kind() {
                        ErrorKind::Verify => format!(
                            "the password from standard input does not open the key {}",
                            path.display()
                        ),
                        _ => could_not_open(err),
                    }
                })
            }
        }
    }?;
    Ok(ReleaseKey {
        secret,
        has_password,
    })
}

// Into one buffer of its own and not with read_to_end, which reads the
// first bytes into a buffer on the stack that nothing wipes.
fn read_password(from: &mut dyn Read) -> Result<String, String> {
    // Room for a byte order mark, a CRLF and one byte more: the longest
    // password fits with what is stripped from it, and a full buffer leaves
    // a password one byte too long however much was stripped.
    let mut bytes = vec![0u8; MOST_PASSWORD_BYTES + 6];
    let mut len = 0;
    while len < bytes.len() {
        match from.read(&mut bytes[len..]) {
            Ok(0) => break,
            Ok(n) => len += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => {
                return Err(format!(
                    "could not read the password from standard input: {err}"
                ));
            }
        }
    }
    let mut password = &bytes[..len];
    // Windows PowerShell puts a byte order mark in front of what it pipes to
    // a program when the console is set to UTF-8.
    password = password.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(password);
    // echo and Get-Content end it with a line break, which is no more part
    // of the password than the Enter that ends it at the console.
    if let Some(line) = password.strip_suffix(b"\n") {
        password = line.strip_suffix(b"\r").unwrap_or(line);
    }
    if password.len() > MOST_PASSWORD_BYTES {
        return Err(format!(
            "the password on standard input is longer than the {MOST_PASSWORD_BYTES} bytes minisign takes, so it is not the key's password"
        ));
    }
    if password.is_empty() {
        return Err("standard input holds no password; pipe in the key's password, or leave out --password-stdin to be asked for it".to_string());
    }
    std::str::from_utf8(password)
        .map(str::to_string)
        .map_err(|_| "the password on standard input is not UTF-8 text".to_string())
}

// A mistyped password at the end of a release should not mean building it
// all again. minisign says a wrong password with a Verify error.
fn with_tries<T>(tries: u32, mut open: impl FnMut() -> minisign::Result<T>) -> minisign::Result<T> {
    let mut tried = 1;
    loop {
        match open() {
            Err(err) if matches!(err.kind(), ErrorKind::Verify) && tried < tries => {
                eprintln!("wrong password for that key, try again");
                tried += 1;
            }
            result => return result,
        }
    }
}

// A minisign secret key line is base64 of "Ed", then the KDF: "Sc" for a
// password-protected key, two zero bytes for none. The first four base64
// characters are exactly the first three bytes.
fn kdf_is_scrypt(line: &str) -> Option<bool> {
    let mut bits = 0u32;
    for c in line.trim().bytes().take(4) {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        bits = bits << 6 | u32::from(value);
    }
    if line.trim().len() < 4 || bits >> 8 != u32::from_be_bytes([0, 0, b'E', b'd']) {
        return None;
    }
    match (bits & 0xFF) as u8 {
        b'S' => Some(true),
        0 => Some(false),
        _ => None,
    }
}

pub fn sign_files(key: &SecretKey, files: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let pk = PublicKey::from_secret_key(key)
        .map_err(|err| format!("could not read the public half of the key: {err}"))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut written = Vec::new();
    for path in files {
        let data =
            File::open(path).map_err(|err| format!("could not open {}: {err}", path.display()))?;
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        // The same trusted comment the minisign tool writes, so either tool
        // shows the same thing when it checks a release by hand.
        let trusted = format!("timestamp:{now}\tfile:{name}\thashed");
        // Given the public key, minisign checks the new signature before
        // returning it.
        let signature = minisign::sign(
            Some(&pk),
            key,
            BufReader::new(data),
            Some(&trusted),
            Some(SIGNATURE_COMMENT),
        )
        .map_err(|err| format!("could not sign {}: {err}", path.display()))?;
        let mut target = path.clone().into_os_string();
        target.push(".minisig");
        let target = PathBuf::from(target);
        fs::write(&target, signature.to_string())
            .map_err(|err| format!("could not write {}: {err}", target.display()))?;
        written.push(target);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use minisign::SignatureBox;

    use super::*;

    struct Folder(PathBuf);

    impl Folder {
        fn new(name: &str) -> Folder {
            let dir =
                std::env::temp_dir().join(format!("booth-release-{name}-{}", std::process::id()));
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

    fn throwaway_key(folder: &Folder) -> (PathBuf, PublicKey) {
        let pair = KeyPair::generate_unencrypted_keypair().unwrap();
        let path = folder.0.join("test.key");
        write_secret(&path, &pair.sk.to_box(Some(KEY_COMMENT)).unwrap()).unwrap();
        (path, pair.pk)
    }

    #[test]
    fn signs_files() {
        let folder = Folder::new("sign");
        let (key_path, pk) = throwaway_key(&folder);
        let manifest = folder.0.join("latest.txt");
        let zip = folder.0.join("booth-0.1.0-windows-x64.zip");
        fs::write(&manifest, "version = 0.1.0\r\n").unwrap();
        fs::write(&zip, vec![7u8; 100_000]).unwrap();

        let key = open_secret_key(&key_path, Password::Console).unwrap();
        assert!(!key.has_password);
        let written = sign_files(&key.secret, &[manifest.clone(), zip.clone()]).unwrap();
        assert_eq!(
            written,
            [
                folder.0.join("latest.txt.minisig"),
                folder.0.join("booth-0.1.0-windows-x64.zip.minisig")
            ]
        );

        let signature = SignatureBox::from_file(&written[0]).unwrap();
        assert!(
            signature
                .trusted_comment()
                .unwrap()
                .contains("file:latest.txt")
        );
        minisign::verify(
            &pk,
            &signature,
            Cursor::new(fs::read(&manifest).unwrap()),
            true,
            false,
            false,
        )
        .unwrap();
        let tampered = b"version = 9.9.9\r\n".to_vec();
        assert!(
            minisign::verify(&pk, &signature, Cursor::new(tampered), true, false, false).is_err()
        );

        let other = KeyPair::generate_unencrypted_keypair().unwrap().pk;
        let signature = SignatureBox::from_file(&written[1]).unwrap();
        assert!(
            minisign::verify(
                &other,
                &signature,
                Cursor::new(fs::read(&zip).unwrap()),
                true,
                false,
                false
            )
            .is_err()
        );
    }

    // What keygen prints is what the update check's constant is made from, so
    // it must be minisign's own public key line and key id.
    #[test]
    fn printed_key_is_minisigns() {
        let pair = KeyPair::generate_unencrypted_keypair().unwrap();
        let line = public_line(&pair.pk);
        let public_box = pair.pk.to_box().unwrap().to_string();
        let mut lines = public_box.lines();
        assert_eq!(
            lines.next().unwrap(),
            format!("untrusted comment: minisign public key: {}", line.id)
        );
        assert_eq!(lines.next().unwrap(), line.base64);
        assert_eq!(PublicKey::from_base64(&line.base64).unwrap(), pair.pk);
        assert_eq!(line.base64.len(), 56);
    }

    #[test]
    fn only_release_key_signs() {
        let folder = Folder::new("app-key");
        let (key_path, pk) = throwaway_key(&folder);
        let key = open_secret_key(&key_path, Password::Console).unwrap();
        let found = PublicKey::from_secret_key(&key.secret).unwrap();
        let same = pk.to_base64();
        assert_eq!(against_app_key(&found, &key_path, &same, false), Ok(None));
        assert_eq!(against_app_key(&found, &key_path, &same, true), Ok(None));

        let other = KeyPair::generate_unencrypted_keypair().unwrap().pk;
        let err = against_app_key(&found, &key_path, &other.to_base64(), false).unwrap_err();
        let (found_line, other_line) = (public_line(&pk), public_line(&other));
        for part in [
            &key_path.display().to_string(),
            &found_line.id,
            &found_line.base64,
            &other_line.id,
            &other_line.base64,
            APP_KEY,
            "sign with the key whose public half is in RELEASE_KEY",
        ] {
            assert!(err.contains(part), "{part} missing from: {err}");
        }
        let warning = against_app_key(&found, &key_path, &other.to_base64(), true)
            .unwrap()
            .unwrap();
        assert!(warning.contains(&other_line.id), "{warning}");
        assert!(warning.contains("never publish"), "{warning}");
    }

    // minisign's == compares the key bytes only. The app also checks the key
    // id, so a key that differs only there is another key.
    #[test]
    fn same_bytes_other_id() {
        let pk = KeyPair::generate_unencrypted_keypair().unwrap().pk;
        let mut bytes = pk.to_bytes();
        bytes[2] ^= 1;
        let twin = PublicKey::from_bytes(&bytes).unwrap();
        assert_eq!(twin, pk);
        let err =
            against_app_key(&pk, Path::new("test.key"), &twin.to_base64(), false).unwrap_err();
        assert!(err.contains(&public_line(&twin).id), "{err}");
    }

    #[test]
    fn release_key_not_made_yet() {
        let pk = KeyPair::generate_unencrypted_keypair().unwrap().pk;
        let path = Path::new(r"E:\booth-release.key");
        let padded = format!("{} ", pk.to_base64());
        for app_key in ["not made yet", "", padded.as_str()] {
            let err = against_app_key(&pk, path, app_key, false).unwrap_err();
            assert!(
                err.contains(&format!("{APP_KEY} is \"{app_key}\", not a public key yet")),
                "{err}"
            );
            assert!(err.contains("-p release -- keygen"), "{err}");
            let warning = against_app_key(&pk, path, app_key, true).unwrap().unwrap();
            assert!(warning.contains("not a public key yet"), "{warning}");
            assert!(!warning.contains("keygen"), "{warning}");
        }
    }

    #[test]
    fn encrypted_or_open_key() {
        let open = KeyPair::generate_unencrypted_keypair()
            .unwrap()
            .sk
            .to_box(None)
            .unwrap()
            .to_string();
        assert_eq!(kdf_is_scrypt(open.lines().nth(1).unwrap()), Some(false));
        // "Ed" then "Sc", as minisign writes a password-protected key.
        assert_eq!(kdf_is_scrypt("RWRTY0IyAAAA"), Some(true));
        assert_eq!(kdf_is_scrypt("not a key"), None);
        assert_eq!(kdf_is_scrypt("RW"), None);
    }

    #[test]
    fn refuses_bad_key_paths() {
        let folder = Folder::new("paths");
        let repository = folder.0.join("repo");
        fs::create_dir_all(repository.join(".git")).unwrap();
        fs::create_dir_all(repository.join("keys")).unwrap();
        let err = check_secret_path(&repository.join("keys").join("booth.key")).unwrap_err();
        assert!(err.contains("inside the git repository"), "{err}");

        let (existing, _) = throwaway_key(&folder);
        assert!(
            check_secret_path(&existing)
                .unwrap_err()
                .contains("never overwrites")
        );
        assert!(
            check_secret_path(&folder.0.join("missing").join("booth.key"))
                .unwrap_err()
                .contains("make the folder first")
        );
        assert!(check_secret_path(&folder.0.join("booth.key")).is_ok());
    }

    #[test]
    fn refuses_a_file_that_is_not_a_key() {
        let folder = Folder::new("not-a-key");
        let path = folder.0.join("junk.key");
        fs::write(&path, "untrusted comment: x\nhello\n").unwrap();
        assert!(
            open_secret_key(&path, Password::Console)
                .err()
                .unwrap()
                .contains("is not a minisign secret key")
        );

        let (key, _) = throwaway_key(&folder);
        let mut big = fs::read(&key).unwrap();
        big.resize(MOST_KEY_BYTES as usize + 1, b'\n');
        fs::write(&path, big).unwrap();
        let err = open_secret_key(&path, Password::Console).err().unwrap();
        assert!(err.contains("larger than a minisign secret key"), "{err}");
    }

    #[test]
    fn refuses_key_in_repository() {
        let folder = Folder::new("sign-in-repo");
        fs::create_dir_all(folder.0.join(".git")).unwrap();
        let (key, _) = throwaway_key(&folder);
        let err = open_secret_key(&key, Password::Console).err().unwrap();
        assert!(err.contains("inside the git repository"), "{err}");
        let mut input = Cursor::new(b"unused".to_vec());
        let err = open_secret_key(&key, Password::Piped(&mut input))
            .err()
            .unwrap();
        assert!(err.contains("inside the git repository"), "{err}");
        assert_eq!(input.position(), 0);
    }

    // A pipe that hands over a byte at a time and is interrupted first.
    struct Trickle<'a> {
        bytes: &'a [u8],
        interrupted: bool,
    }

    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(io::ErrorKind::Interrupted.into());
            }
            let Some((first, rest)) = self.bytes.split_first() else {
                return Ok(0);
            };
            buf[0] = *first;
            self.bytes = rest;
            Ok(1)
        }
    }

    fn read(input: &[u8]) -> Result<String, String> {
        read_password(&mut Cursor::new(input))
    }

    // As tools\release.ps1 pipes it from Windows Credential Manager, UTF-8
    // with nothing around it, and as echo or Windows PowerShell may.
    #[test]
    fn reads_piped_password() {
        let password = "caf\u{e9} au lait 7";
        for input in [
            password.to_string(),
            format!("{password}\n"),
            format!("{password}\r\n"),
            format!("\u{feff}{password}"),
            format!("\u{feff}{password}\r\n"),
        ] {
            assert_eq!(read(input.as_bytes()).as_deref(), Ok(password));
        }
        // Only one line break goes, and spaces are the password's own, as
        // they are when it is typed.
        assert_eq!(read(b" x \n\n").as_deref(), Ok(" x \n"));
        let longest = "a".repeat(MOST_PASSWORD_BYTES);
        for input in [longest.clone(), format!("\u{feff}{longest}\r\n")] {
            assert_eq!(read(input.as_bytes()).as_deref(), Ok(longest.as_str()));
        }
        let mut trickle = Trickle {
            bytes: password.as_bytes(),
            interrupted: false,
        };
        assert_eq!(read_password(&mut trickle).as_deref(), Ok(password));
    }

    #[test]
    fn refuses_what_is_not_a_password() {
        let too_long = "a".repeat(MOST_PASSWORD_BYTES + 1);
        let around = format!("\u{feff}{too_long}\r\n");
        let fills_buffer = "a".repeat(MOST_PASSWORD_BYTES + 6);
        let far_too_long = format!("\u{feff}{}\r\n", "a".repeat(70_000));
        let cases: [(&[u8], &str); 8] = [
            (b"", "holds no password"),
            (b"\r\n", "holds no password"),
            (b"\xEF\xBB\xBF\n", "holds no password"),
            (b"r\0i\0g\0h\0t\0\xFF", "not UTF-8"),
            (too_long.as_bytes(), "longer than the 1024 bytes"),
            (around.as_bytes(), "longer than the 1024 bytes"),
            (fills_buffer.as_bytes(), "longer than the 1024 bytes"),
            (far_too_long.as_bytes(), "longer than the 1024 bytes"),
        ];
        for (input, says) in cases {
            let err = read(input).unwrap_err();
            assert!(err.contains(says), "{says} missing from: {err}");
        }
    }

    #[test]
    fn opens_with_piped_password() {
        let folder = Folder::new("piped");
        let password = "caf\u{e9} au lait 7";
        let pair = KeyPair::generate_encrypted_keypair(Some(password.to_string())).unwrap();
        let key_path = folder.0.join("locked.key");
        write_secret(&key_path, &pair.sk.to_box(Some(KEY_COMMENT)).unwrap()).unwrap();
        let piped =
            |input: &[u8]| open_secret_key(&key_path, Password::Piped(&mut Cursor::new(input)));

        let key = piped(format!("{password}\r\n").as_bytes()).unwrap();
        assert!(key.has_password);
        assert_eq!(PublicKey::from_secret_key(&key.secret).unwrap(), pair.pk);

        // The whole pipe is the password, read once, so a second line is not
        // a second try. Nothing read from it goes into the error.
        let err = piped(format!("wrong\r\n{password}\r\n").as_bytes())
            .err()
            .unwrap();
        assert_eq!(
            err,
            format!(
                "the password from standard input does not open the key {}",
                key_path.display()
            )
        );
        let err = piped(b"").err().unwrap();
        assert!(err.contains("holds no password"), "{err}");

        // A key without a password has nothing to read.
        let (open_path, _) = throwaway_key(&folder);
        let mut input = Cursor::new(b"unused".to_vec());
        let key = open_secret_key(&open_path, Password::Piped(&mut input)).unwrap();
        assert!(!key.has_password);
        assert_eq!(input.position(), 0);
    }

    fn wrong_password() -> minisign::PError {
        minisign::PError::new(ErrorKind::Verify, "Wrong password for that key")
    }

    #[test]
    fn password_tries() {
        let mut calls = 0;
        let opened = with_tries(3, || {
            calls += 1;
            if calls < 3 {
                Err(wrong_password())
            } else {
                Ok(calls)
            }
        });
        assert_eq!(opened.unwrap(), 3);

        let mut calls = 0;
        let opened: minisign::Result<()> = with_tries(3, || {
            calls += 1;
            Err(wrong_password())
        });
        assert!(opened.unwrap_err().to_string().contains("Wrong password"));
        assert_eq!(calls, 3);

        // Anything else, such as a console that cannot be read, is not
        // asked again.
        let mut calls = 0;
        let opened: minisign::Result<()> = with_tries(3, || {
            calls += 1;
            Err(minisign::PError::new(ErrorKind::Io, "no console"))
        });
        assert!(opened.is_err());
        assert_eq!(calls, 1);
    }
}
