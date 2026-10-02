// The zip is fetched into the data folder and hashed as it arrives, and it
// is moved into Downloads only once its SHA-256 matches the signed
// manifest, so a half-fetched or altered file never sits there under the
// release's name. Nothing here runs or unzips anything.

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, ERROR_NOT_SAME_DEVICE,
};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_HASH_HANDLE, BCRYPT_SHA256_ALG_HANDLE, BCryptCreateHash, BCryptDestroyHash,
    BCryptFinishHash, BCryptHashData,
};
use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::UI::Shell::{FOLDERID_Downloads, KF_FLAG_CREATE, SHGetKnownFolderPath};

use super::http::{self, FetchError, Rules};
use super::manifest::Manifest;

// The first release is about 40 MB zipped, most of it FFmpeg. This leaves
// room for later ones and still stops a server that would fill the disk.
pub const MOST_ZIP_BYTES: u64 = 256 * 1024 * 1024;
// A read that waits this long means the line has stalled.
const READ_WAIT: Duration = Duration::from_secs(30);
const PART: &str = "download.part";
// Added to the zip's name while it is copied into Downloads on another
// drive. No browser uses it, so only Booth ever makes a file of that name.
const COPYING: &str = ".copying";
const COPY_CHUNK: usize = 64 * 1024;

// SHA-256 from Windows' own cryptography (CNG), so no hashing crate is
// needed.
pub struct Sha256(BCRYPT_HASH_HANDLE);

impl Sha256 {
    pub fn new() -> io::Result<Sha256> {
        let mut handle: BCRYPT_HASH_HANDLE = ptr::null_mut();
        // SAFETY: the SHA-256 pseudo-handle needs no opening; with no hash
        // object buffer CNG allocates its own, freed by BCryptDestroyHash.
        let status = unsafe {
            BCryptCreateHash(
                BCRYPT_SHA256_ALG_HANDLE,
                &mut handle,
                ptr::null_mut(),
                0,
                ptr::null(),
                0,
                0,
            )
        };
        cng(status, "start a SHA-256")?;
        Ok(Sha256(handle))
    }

    pub fn update(&mut self, bytes: &[u8]) -> io::Result<()> {
        for chunk in bytes.chunks(u32::MAX as usize) {
            // SAFETY: a live hash, and a chunk no longer than u32 can say.
            let status = unsafe { BCryptHashData(self.0, chunk.as_ptr(), chunk.len() as u32, 0) };
            cng(status, "hash the download")?;
        }
        Ok(())
    }

    pub fn finish(self) -> io::Result<[u8; 32]> {
        let mut digest = [0u8; 32];
        // SAFETY: a live hash and an output of SHA-256's 32 bytes.
        let status = unsafe { BCryptFinishHash(self.0, digest.as_mut_ptr(), 32, 0) };
        cng(status, "finish the SHA-256")?;
        Ok(digest)
    }
}

impl Drop for Sha256 {
    fn drop(&mut self) {
        // SAFETY: made by BCryptCreateHash and destroyed once, here.
        unsafe { BCryptDestroyHash(self.0) };
    }
}

fn cng(status: i32, what: &str) -> io::Result<()> {
    if status < 0 {
        return Err(io::Error::other(format!(
            "could not {what}: Windows cryptography answered {status:#010x}"
        )));
    }
    Ok(())
}

// None for a file longer than `most`, which is not read past it.
fn file_sha256(path: &Path, most: u64) -> io::Result<Option<[u8; 32]>> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new()?;
    let mut buffer = vec![0u8; COPY_CHUNK];
    let mut total = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return hash.finish().map(Some);
        }
        total += read as u64;
        if total > most {
            return Ok(None);
        }
        hash.update(&buffer[..read])?;
    }
}

pub fn part_path(dir: &Path) -> PathBuf {
    dir.join(PART)
}

// What Booth itself was run from, for "unzip it over this copy".
pub fn own_folder() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
}

pub fn downloads_folder() -> io::Result<PathBuf> {
    let mut raw: *mut u16 = ptr::null_mut();
    // SAFETY: a known folder id, no token (this user), and an out pointer
    // Windows fills with a string it allocated.
    let result = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_Downloads,
            KF_FLAG_CREATE as u32,
            ptr::null_mut(),
            &mut raw,
        )
    };
    let path = if result < 0 || raw.is_null() {
        Err(io::Error::from_raw_os_error(result))
    } else {
        // SAFETY: on success raw is a zero-terminated UTF-16 string.
        let len = (0..).take_while(|&i| unsafe { *raw.add(i) } != 0).count();
        // SAFETY: len units were just read from it.
        let units = unsafe { std::slice::from_raw_parts(raw, len) };
        Ok(PathBuf::from(OsString::from_wide(units)))
    };
    // SAFETY: Windows asks for the string to be freed whether the call
    // worked or not; freeing null does nothing.
    unsafe { CoTaskMemFree(raw.cast()) };
    path
}

#[derive(Debug, PartialEq, Eq)]
pub enum Kept {
    Saved(PathBuf),
    // A file of that name with the signed release's hash was already there.
    AlreadyThere(PathBuf),
}

#[derive(Debug)]
pub enum DownloadError {
    Fetch(FetchError),
    // Could not be written or read back in the data folder.
    Part { path: PathBuf, source: io::Error },
    Mismatch,
    NoDownloads(io::Error),
    // Another file of the same name is in Downloads.
    Taken(PathBuf),
    Move { to: PathBuf, source: io::Error },
}

impl fmt::Display for DownloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DownloadError::Fetch(err) => write!(f, "{err}"),
            DownloadError::Part { path, source } => {
                write!(f, "could not write {}: {source}", path.display())
            }
            DownloadError::Mismatch => write!(f, "the download does not match the signed release"),
            DownloadError::NoDownloads(err) => {
                write!(
                    f,
                    "Windows did not say where the Downloads folder is: {err}"
                )
            }
            DownloadError::Taken(path) => {
                write!(f, "a different file is already at {}", path.display())
            }
            DownloadError::Move { to, source } => {
                write!(
                    f,
                    "could not move the download to {}: {source}",
                    to.display()
                )
            }
        }
    }
}

// Fetches the release into the data folder, then keeps it in `downloads`
// if it is the one the manifest names. The same release already there, or
// another file in its place, is found before anything is fetched and again
// after. `progress` hears the bytes so far and the length the server gave,
// if it gave one.
pub fn fetch(
    manifest: &Manifest,
    dir: &Path,
    downloads: &Path,
    stop: &AtomicBool,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<Kept, DownloadError> {
    if let Some(kept) = look(&downloads.join(&manifest.zip), &manifest.sha256)? {
        return Ok(kept);
    }
    let part = part_path(dir);
    let rules = Rules {
        most: MOST_ZIP_BYTES,
        wait: READ_WAIT,
        deadline: None,
        fresh: false,
    };
    let digest = receive(manifest, &part, &rules, stop, progress).inspect_err(|_| {
        let _ = fs::remove_file(&part);
    })?;
    keep(&part, digest, manifest, downloads)
}

// None when the zip's place in Downloads is free. A file there that is not
// the release, or cannot be read, is not Booth's to replace.
fn look(to: &Path, sha256: &[u8; 32]) -> Result<Option<Kept>, DownloadError> {
    match file_sha256(to, MOST_ZIP_BYTES) {
        Ok(Some(there)) if there == *sha256 => Ok(Some(Kept::AlreadyThere(to.to_path_buf()))),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Ok(_) | Err(_) => Err(DownloadError::Taken(to.to_path_buf())),
    }
}

fn receive(
    manifest: &Manifest,
    part: &Path,
    rules: &Rules,
    stop: &AtomicBool,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<[u8; 32], DownloadError> {
    let failed = |source| DownloadError::Part {
        path: part.to_path_buf(),
        source,
    };
    if stop.load(Ordering::Acquire) {
        return Err(DownloadError::Fetch(FetchError::Stopped));
    }
    let mut file = File::create(part).map_err(failed)?;
    let mut hash = Sha256::new().map_err(failed)?;
    let mut body = http::open(&manifest.url, rules, stop).map_err(DownloadError::Fetch)?;
    let length = body.length();
    let mut got = 0u64;
    http::pour_body(&mut body, rules.most, stop, &mut |chunk| {
        file.write_all(chunk)?;
        hash.update(chunk)?;
        got += chunk.len() as u64;
        progress(got, length);
        Ok(())
    })
    .map_err(DownloadError::Fetch)?;
    file.sync_all().map_err(failed)?;
    hash.finish().map_err(failed)
}

// Every way out but a keep leaves no part file behind.
pub fn keep(
    part: &Path,
    digest: [u8; 32],
    manifest: &Manifest,
    downloads: &Path,
) -> Result<Kept, DownloadError> {
    let dropped = |err| {
        let _ = fs::remove_file(part);
        Err(err)
    };
    if digest != manifest.sha256 {
        return dropped(DownloadError::Mismatch);
    }
    let to = downloads.join(&manifest.zip);
    match look(&to, &manifest.sha256) {
        Ok(Some(kept)) => {
            let _ = fs::remove_file(part);
            return Ok(kept);
        }
        Ok(None) => {}
        Err(err) => return dropped(err),
    }
    let placed = match rename_new(part, &to) {
        Ok(()) => Ok(()),
        Err(err) if err.raw_os_error() == Some(ERROR_NOT_SAME_DEVICE as i32) => {
            copy_across(part, &to, &manifest.sha256)
        }
        Err(err) => Err(move_failed(&to, err)),
    };
    match placed {
        Ok(()) => {
            let _ = fs::remove_file(part);
            Ok(Kept::Saved(to))
        }
        Err(err) => dropped(err),
    }
}

// A rename on one drive, which is whole or not at all, and never over a
// file that appeared since the look before it.
fn rename_new(from: &Path, to: &Path) -> io::Result<()> {
    let (from, target) = (wide(from), wide(to));
    // SAFETY: both paths are zero terminated and outlive the call.
    if unsafe { MoveFileExW(from.as_ptr(), target.as_ptr(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn move_failed(to: &Path, source: io::Error) -> DownloadError {
    let exists = [ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS]
        .iter()
        .any(|code| source.raw_os_error() == Some(*code as i32));
    if exists {
        return DownloadError::Taken(to.to_path_buf());
    }
    DownloadError::Move {
        to: to.to_path_buf(),
        source,
    }
}

// Downloads on another drive cannot take a rename, so the zip is copied
// next to it under a name of Booth's own and renamed once it is whole: a
// copy cut off when Booth closes never sits there under the release's name.
// The bytes are hashed again on the way, so the file that gets that name
// is the one that matched.
fn copy_across(part: &Path, to: &Path, sha256: &[u8; 32]) -> Result<(), DownloadError> {
    let mut copying = to.as_os_str().to_owned();
    copying.push(COPYING);
    let copying = PathBuf::from(copying);
    // Left by a copy that was cut off.
    let _ = fs::remove_file(&copying);
    let copied = copy_checked(part, &copying, to, sha256)
        .and_then(|()| rename_new(&copying, to).map_err(|err| move_failed(to, err)));
    if copied.is_err() {
        let _ = fs::remove_file(&copying);
    }
    copied
}

fn copy_checked(
    part: &Path,
    copying: &Path,
    to: &Path,
    sha256: &[u8; 32],
) -> Result<(), DownloadError> {
    let unread = |source| DownloadError::Part {
        path: part.to_path_buf(),
        source,
    };
    let unwritten = |source| DownloadError::Move {
        to: to.to_path_buf(),
        source,
    };
    let mut from = File::open(part).map_err(unread)?;
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(copying)
        .map_err(unwritten)?;
    let mut hash = Sha256::new().map_err(unwritten)?;
    let mut buffer = vec![0u8; COPY_CHUNK];
    loop {
        let read = from.read(&mut buffer).map_err(unread)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]).map_err(unwritten)?;
        out.write_all(&buffer[..read]).map_err(unwritten)?;
    }
    if hash.finish().map_err(unwritten)? != *sha256 {
        return Err(DownloadError::Mismatch);
    }
    out.sync_all().map_err(unwritten)
}

// The mark a browser puts on a download, so Windows treats the zip, and
// the files unzipped from it, as coming from the internet: SmartScreen
// looks at the new booth.exe as it did at the first one. A disk that has
// no room for the mark (FAT32, exFAT) keeps the file without it.
pub fn mark_from_internet(path: &Path, url: &str) -> io::Result<()> {
    let mut stream = path.as_os_str().to_owned();
    stream.push(":Zone.Identifier");
    fs::write(
        PathBuf::from(stream),
        format!("[ZoneTransfer]\r\nZoneId=3\r\nHostUrl={url}\r\n"),
    )
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain([0]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::manifest::{self, hex, tests::NEWER, tests::NEWER_ZIP};

    struct Folder(PathBuf);

    impl Folder {
        fn new(name: &str) -> Folder {
            let path =
                std::env::temp_dir().join(format!("booth-update-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(path.join("data")).unwrap();
            fs::create_dir_all(path.join("Downloads")).unwrap();
            Folder(path)
        }

        fn data(&self) -> PathBuf {
            self.0.join("data")
        }

        fn downloads(&self) -> PathBuf {
            self.0.join("Downloads")
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sha256(bytes: &[u8]) -> [u8; 32] {
        let mut hash = Sha256::new().unwrap();
        hash.update(bytes).unwrap();
        hash.finish().unwrap()
    }

    // The part file as a fetch leaves it: the bytes, and their hash taken
    // on the way.
    fn arrived(folder: &Folder, bytes: &[u8]) -> (PathBuf, [u8; 32]) {
        let part = part_path(&folder.data());
        fs::write(&part, bytes).unwrap();
        (part, sha256(bytes))
    }

    fn newer() -> Manifest {
        manifest::parse(NEWER.as_bytes()).unwrap()
    }

    #[test]
    fn sha256_matches_the_published_vectors() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let mut hash = Sha256::new().unwrap();
        for _ in 0..1000 {
            hash.update(&[b'a'; 1000]).unwrap();
        }
        assert_eq!(
            hex(&hash.finish().unwrap()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn matching_download_moved_to_downloads() {
        let folder = Folder::new("matches");
        let (part, digest) = arrived(&folder, NEWER_ZIP);
        let kept = keep(&part, digest, &newer(), &folder.downloads()).unwrap();
        let to = folder.downloads().join("booth-0.2.0-windows-x64.zip");
        assert_eq!(kept, Kept::Saved(to.clone()));
        assert_eq!(fs::read(&to).unwrap(), NEWER_ZIP);
        assert!(!part.exists());
        mark_from_internet(&to, &newer().url).unwrap();
        let mut mark = to.into_os_string();
        mark.push(":Zone.Identifier");
        let mark = fs::read_to_string(PathBuf::from(mark)).unwrap();
        assert!(mark.starts_with("[ZoneTransfer]\r\nZoneId=3\r\n"), "{mark}");
    }

    #[test]
    fn a_download_that_does_not_match_is_deleted() {
        let folder = Folder::new("mismatch");
        let mut altered = NEWER_ZIP.to_vec();
        altered[0] ^= 1;
        let (part, digest) = arrived(&folder, &altered);
        let err = keep(&part, digest, &newer(), &folder.downloads()).unwrap_err();
        assert!(matches!(err, DownloadError::Mismatch), "{err}");
        assert!(!part.exists());
        assert_eq!(fs::read_dir(folder.downloads()).unwrap().count(), 0);
    }

    // The hash that counts is the one taken as the bytes arrived, not the
    // file read back afterwards.
    #[test]
    fn hash_taken_on_the_way() {
        let folder = Folder::new("on-the-way");
        let (part, _) = arrived(&folder, NEWER_ZIP);
        let err = keep(&part, [0; 32], &newer(), &folder.downloads()).unwrap_err();
        assert!(matches!(err, DownloadError::Mismatch), "{err}");
        assert!(!part.exists());
    }

    #[test]
    fn other_file_of_same_name_left_alone() {
        let folder = Folder::new("taken");
        let to = folder.downloads().join("booth-0.2.0-windows-x64.zip");
        fs::write(&to, b"someone else's file").unwrap();
        let (part, digest) = arrived(&folder, NEWER_ZIP);
        let err = keep(&part, digest, &newer(), &folder.downloads()).unwrap_err();
        assert!(
            matches!(&err, DownloadError::Taken(path) if *path == to),
            "{err}"
        );
        assert_eq!(fs::read(&to).unwrap(), b"someone else's file");
        assert!(!part.exists());
    }

    #[test]
    fn same_release_already_there() {
        let folder = Folder::new("already");
        let to = folder.downloads().join("booth-0.2.0-windows-x64.zip");
        fs::write(&to, NEWER_ZIP).unwrap();
        let (part, digest) = arrived(&folder, NEWER_ZIP);
        let kept = keep(&part, digest, &newer(), &folder.downloads()).unwrap();
        assert_eq!(kept, Kept::AlreadyThere(to));
        assert!(!part.exists());
    }

    // The stop set here would end a fetch that went ahead before it
    // reached the network, so this test never does.
    #[test]
    fn downloads_is_looked_at_before_anything_is_fetched() {
        let folder = Folder::new("before");
        let stop = AtomicBool::new(true);
        let to = folder.downloads().join("booth-0.2.0-windows-x64.zip");
        let press = || {
            fetch(
                &newer(),
                &folder.data(),
                &folder.downloads(),
                &stop,
                &mut |_, _| {},
            )
        };
        let err = press().unwrap_err();
        assert!(
            matches!(err, DownloadError::Fetch(FetchError::Stopped)),
            "{err}"
        );
        assert!(!part_path(&folder.data()).exists());

        fs::write(&to, NEWER_ZIP).unwrap();
        assert_eq!(press().unwrap(), Kept::AlreadyThere(to.clone()));

        fs::write(&to, b"someone else's file").unwrap();
        let err = press().unwrap_err();
        assert!(
            matches!(&err, DownloadError::Taken(path) if *path == to),
            "{err}"
        );
        assert_eq!(fs::read(&to).unwrap(), b"someone else's file");
    }

    #[test]
    fn copy_to_another_drive() {
        let folder = Folder::new("across");
        let (part, _) = arrived(&folder, NEWER_ZIP);
        let to = folder.downloads().join("booth-0.2.0-windows-x64.zip");
        let copying = folder
            .downloads()
            .join("booth-0.2.0-windows-x64.zip.copying");
        let sha256 = newer().sha256;

        fs::write(&copying, b"left by a copy that was cut off").unwrap();
        copy_across(&part, &to, &sha256).unwrap();
        assert_eq!(fs::read(&to).unwrap(), NEWER_ZIP);
        assert!(!copying.exists());

        // A file that took the release's name meanwhile stays as it is.
        let err = copy_across(&part, &to, &sha256).unwrap_err();
        assert!(
            matches!(&err, DownloadError::Taken(path) if *path == to),
            "{err}"
        );
        assert_eq!(fs::read(&to).unwrap(), NEWER_ZIP);
        assert!(!copying.exists());

        // The part changed on disk after it was hashed on the way.
        fs::remove_file(&to).unwrap();
        fs::write(&part, b"changed after it arrived").unwrap();
        let err = copy_across(&part, &to, &sha256).unwrap_err();
        assert!(matches!(err, DownloadError::Mismatch), "{err}");
        assert!(!to.exists());
        assert!(!copying.exists());
    }

    #[test]
    fn missing_downloads_folder() {
        let folder = Folder::new("no-folder");
        let (part, digest) = arrived(&folder, NEWER_ZIP);
        let missing = folder.0.join("no such folder");
        let err = keep(&part, digest, &newer(), &missing).unwrap_err();
        assert!(matches!(err, DownloadError::Move { .. }), "{err}");
        assert!(!part.exists());
    }

    #[test]
    fn windows_says_where_downloads_is() {
        let downloads = downloads_folder().unwrap();
        assert!(downloads.is_absolute(), "{}", downloads.display());
    }
}
