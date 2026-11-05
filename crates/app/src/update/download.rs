// The zip is fetched into the data folder and hashed as it arrives, and it
// is moved into Downloads only once its SHA-256 matches the signed
// manifest, so a half-fetched or altered file never sits there under the
// release's name. Nothing here runs or unzips anything.

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, ERROR_NOT_SAME_DEVICE, GENERIC_READ, GENERIC_WRITE,
};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_HASH_HANDLE, BCRYPT_SHA256_ALG_HANDLE, BCryptCreateHash, BCryptDestroyHash,
    BCryptFinishHash, BCryptHashData,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX, FILE_RENAME_INFO, FILE_SHARE_READ,
    FileDispositionInfo, FileDispositionInfoEx, FileRenameInfo, SetFileInformationByHandle,
};
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
const COPYING: &str = ".copying";
// Staging names tried before the copy gives up with the last reason.
const COPYING_NAMES: u64 = 16;
const COPY_CHUNK: usize = 64 * 1024;

struct Pending {
    file: File,
    published: bool,
}

impl Pending {
    fn create(path: &Path, new: bool) -> io::Result<Pending> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
            .share_mode(FILE_SHARE_READ)
            .create(!new)
            .truncate(!new)
            .create_new(new)
            .open(path)?;
        let pending = Pending {
            file,
            published: false,
        };
        if new {
            pending.delete_on_close(true)?;
        }
        Ok(pending)
    }

    fn publish(&mut self, to: &Path) -> io::Result<()> {
        self.delete_on_close(false)?;
        rename_new(&self.file, to)?;
        self.published = true;
        Ok(())
    }

    fn delete_on_close(&self, delete: bool) -> io::Result<()> {
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: delete };
        // SAFETY: a live handle with delete access and a correctly sized
        // structure. Using the handle cannot delete a replacement file.
        let ok = unsafe {
            SetFileInformationByHandle(
                self.file.as_raw_handle(),
                FileDispositionInfo,
                (&disposition as *const FILE_DISPOSITION_INFO).cast(),
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if !self.published {
            let disposition = FILE_DISPOSITION_INFO_EX {
                Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
            };
            // SAFETY: a live handle with delete access and a correctly sized
            // structure. Using the handle cannot delete a replacement file.
            let ok = unsafe {
                SetFileInformationByHandle(
                    self.file.as_raw_handle(),
                    FileDispositionInfoEx,
                    (&disposition as *const FILE_DISPOSITION_INFO_EX).cast(),
                    size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
                )
            };
            if ok == 0 {
                let _ = self.delete_on_close(true);
            }
        }
    }
}

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
    let (file, digest) = receive(manifest, &part, &rules, stop, progress)?;
    keep(&part, file, digest, manifest, downloads)
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
) -> Result<(Pending, [u8; 32]), DownloadError> {
    let failed = |source| DownloadError::Part {
        path: part.to_path_buf(),
        source,
    };
    if stop.load(Ordering::Acquire) {
        return Err(DownloadError::Fetch(FetchError::Stopped));
    }
    let mut file = Pending::create(part, false).map_err(failed)?;
    let mut hash = Sha256::new().map_err(failed)?;
    let mut body = http::open(&manifest.url, rules, stop).map_err(DownloadError::Fetch)?;
    let length = body.length();
    let mut got = 0u64;
    http::pour_body(&mut body, rules.most, stop, &mut |chunk| {
        file.file.write_all(chunk)?;
        hash.update(chunk)?;
        got += chunk.len() as u64;
        progress(got, length);
        Ok(())
    })
    .map_err(DownloadError::Fetch)?;
    file.file.sync_all().map_err(failed)?;
    let digest = hash.finish().map_err(failed)?;
    Ok((file, digest))
}

// Removal of an unpublished part is best effort: Windows may refuse it.
fn keep(
    part: &Path,
    mut file: Pending,
    digest: [u8; 32],
    manifest: &Manifest,
    downloads: &Path,
) -> Result<Kept, DownloadError> {
    if digest != manifest.sha256 {
        return Err(DownloadError::Mismatch);
    }
    let to = downloads.join(&manifest.zip);
    match look(&to, &manifest.sha256) {
        Ok(Some(kept)) => return Ok(kept),
        Ok(None) => {}
        Err(err) => return Err(err),
    }
    match file.publish(&to) {
        Ok(()) => Ok(()),
        Err(err) if err.raw_os_error() == Some(ERROR_NOT_SAME_DEVICE as i32) => {
            copy_across(part, &to, &manifest.sha256)
        }
        Err(err) => Err(move_failed(&to, err)),
    }?;
    Ok(Kept::Saved(to))
}

// A rename on one drive, which is whole or not at all, and never over a
// file that appeared since the look before it.
fn rename_new(from: &File, to: &Path) -> io::Result<()> {
    let target = wide(&std::path::absolute(to)?);
    let size = size_of::<FILE_RENAME_INFO>() + size_of_val(target.as_slice());
    let size = u32::try_from(size).map_err(|_| io::ErrorKind::InvalidInput)?;
    let mut buffer =
        vec![FILE_RENAME_INFO::default(); (size as usize).div_ceil(size_of::<FILE_RENAME_INFO>())];
    let info = buffer.as_mut_ptr();
    // SAFETY: the buffer is aligned for the structure and has room for the
    // whole name. Zeroed flags forbid replacement; the source stays locked.
    let ok = unsafe {
        (*info).FileNameLength = ((target.len() - 1) * size_of::<u16>()) as u32;
        ptr::copy_nonoverlapping(
            target.as_ptr(),
            ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
            target.len(),
        );
        SetFileInformationByHandle(from.as_raw_handle(), FileRenameInfo, info.cast(), size)
    };
    if ok == 0 {
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
    let mut number = 0u64;
    let mut out = loop {
        let mut copying = to.as_os_str().to_owned();
        copying.push(COPYING);
        if number != 0 {
            copying.push(format!(".{number}"));
        }
        // Another copy's staging file is delete pending while it lives, and
        // Windows answers access denied for that name, not already exists.
        match Pending::create(Path::new(&copying), true) {
            Ok(out) => break out,
            Err(err)
                if number + 1 < COPYING_NAMES
                    && matches!(
                        err.kind(),
                        io::ErrorKind::AlreadyExists | io::ErrorKind::PermissionDenied
                    ) =>
            {
                number += 1
            }
            Err(source) => {
                return Err(DownloadError::Move {
                    to: to.to_path_buf(),
                    source,
                });
            }
        }
    };
    copy_checked(part, &mut out.file, to, sha256)?;
    out.publish(to).map_err(|err| move_failed(to, err))
}

fn copy_checked(
    part: &Path,
    out: &mut File,
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
    fn arrived(folder: &Folder, bytes: &[u8]) -> (PathBuf, Pending, [u8; 32]) {
        let part = part_path(&folder.data());
        let mut file = Pending::create(&part, false).unwrap();
        file.file.write_all(bytes).unwrap();
        (part, file, sha256(bytes))
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
        let (part, file, digest) = arrived(&folder, NEWER_ZIP);
        let kept = keep(&part, file, digest, &newer(), &folder.downloads()).unwrap();
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
    fn the_download_stays_locked_until_publication() {
        let folder = Folder::new("locked");
        let (part, file, digest) = arrived(&folder, NEWER_ZIP);
        let replacement = folder.data().join("replacement");
        fs::write(&replacement, b"a replacement file").unwrap();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                barrier.wait();
                let changed = fs::write(&part, b"changed after it arrived");
                let replaced = fs::rename(&replacement, &part);
                let removed = fs::remove_file(&part);
                (changed, replaced, removed)
            });
            barrier.wait();
            let (changed, replaced, removed) = writer.join().unwrap();
            assert!(changed.is_err(), "the received bytes could be changed");
            assert!(replaced.is_err(), "the received file could be replaced");
            assert!(removed.is_err(), "the received file could be removed");
        });
        let to = folder.downloads().join(&newer().zip);
        assert_eq!(
            keep(&part, file, digest, &newer(), &folder.downloads()).unwrap(),
            Kept::Saved(to.clone())
        );
        assert_eq!(fs::read(to).unwrap(), NEWER_ZIP);
    }

    #[test]
    fn a_stale_part_is_replaced() {
        let folder = Folder::new("existing-part");
        let part = part_path(&folder.data());
        fs::write(&part, b"a download that was cut off before it could finish").unwrap();
        let (part, file, digest) = arrived(&folder, NEWER_ZIP);
        assert_eq!(fs::read(&part).unwrap(), NEWER_ZIP);
        let to = folder.downloads().join(&newer().zip);
        assert_eq!(
            keep(&part, file, digest, &newer(), &folder.downloads()).unwrap(),
            Kept::Saved(to.clone())
        );
        assert_eq!(fs::read(to).unwrap(), NEWER_ZIP);
        assert!(!part.exists());
    }

    #[test]
    fn an_unpublished_part_can_be_reused_while_it_is_read() {
        let folder = Folder::new("part-reader");
        let (part, file, _) = arrived(&folder, NEWER_ZIP);
        let mut reader = File::open(&part).unwrap();
        drop(file);
        let mut next = Pending::create(&part, false).unwrap();
        next.file.write_all(b"the next download").unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, NEWER_ZIP);
        assert_eq!(fs::read(&part).unwrap(), b"the next download");
    }

    #[test]
    fn a_process_closing_removes_its_unpublished_copy() {
        let key = "BOOTH_TEST_COPYING";
        if let Some(path) = std::env::var_os(key) {
            let mut file = Pending::create(Path::new(&path), true).unwrap();
            file.file
                .write_all(b"a copy cut off before publication")
                .unwrap();
            std::process::exit(0);
        }
        let folder = Folder::new("copying-closed");
        let copying = folder
            .downloads()
            .join("booth-0.2.0-windows-x64.zip.copying");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "update::download::tests::a_process_closing_removes_its_unpublished_copy",
                "--exact",
                "--nocapture",
            ])
            .env(key, &copying)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{}\n{stdout}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(stdout.contains("running 1 test"), "{stdout}");
        assert!(!copying.exists());
    }

    #[test]
    fn a_download_that_does_not_match_is_deleted() {
        let folder = Folder::new("mismatch");
        let mut altered = NEWER_ZIP.to_vec();
        altered[0] ^= 1;
        let (part, file, digest) = arrived(&folder, &altered);
        let err = keep(&part, file, digest, &newer(), &folder.downloads()).unwrap_err();
        assert!(matches!(err, DownloadError::Mismatch), "{err}");
        assert!(!part.exists());
        assert_eq!(fs::read_dir(folder.downloads()).unwrap().count(), 0);
    }

    #[test]
    #[allow(clippy::permissions_set_readonly_false)]
    fn a_mismatch_does_not_claim_that_cleanup_succeeded() {
        let folder = Folder::new("mismatch-left");
        let (part, file, _) = arrived(&folder, b"not the release");
        let mut permissions = file.file.metadata().unwrap().permissions();
        permissions.set_readonly(true);
        file.file.set_permissions(permissions).unwrap();
        let err = keep(&part, file, [0; 32], &newer(), &folder.downloads()).unwrap_err();
        assert!(matches!(err, DownloadError::Mismatch), "{err}");
        assert!(part.exists());
        let mut permissions = fs::metadata(&part).unwrap().permissions();
        permissions.set_readonly(false);
        fs::set_permissions(&part, permissions).unwrap();
        assert_eq!(
            super::super::download_failed(&newer(), &err),
            "The download did not match the signed release. Press Download to try again."
        );
        assert_eq!(fs::read_dir(folder.downloads()).unwrap().count(), 0);
    }

    // The hash that counts is the one taken as the bytes arrived, not the
    // file read back afterwards.
    #[test]
    fn hash_taken_on_the_way() {
        let folder = Folder::new("on-the-way");
        let (part, file, _) = arrived(&folder, NEWER_ZIP);
        let err = keep(&part, file, [0; 32], &newer(), &folder.downloads()).unwrap_err();
        assert!(matches!(err, DownloadError::Mismatch), "{err}");
        assert!(!part.exists());
    }

    #[test]
    fn other_file_of_same_name_left_alone() {
        let folder = Folder::new("taken");
        let to = folder.downloads().join("booth-0.2.0-windows-x64.zip");
        fs::write(&to, b"someone else's file").unwrap();
        let (part, file, digest) = arrived(&folder, NEWER_ZIP);
        let err = keep(&part, file, digest, &newer(), &folder.downloads()).unwrap_err();
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
        let (part, file, digest) = arrived(&folder, NEWER_ZIP);
        let kept = keep(&part, file, digest, &newer(), &folder.downloads()).unwrap();
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
        let (part, file, _) = arrived(&folder, NEWER_ZIP);
        let to = folder.downloads().join("booth-0.2.0-windows-x64.zip");
        let copying = folder
            .downloads()
            .join("booth-0.2.0-windows-x64.zip.copying");
        let sha256 = newer().sha256;

        copy_across(&part, &to, &sha256).unwrap();
        assert_eq!(fs::read(&to).unwrap(), NEWER_ZIP);
        assert!(!copying.exists());
        assert!(part.exists());

        // A file that took the release's name meanwhile stays as it is.
        let err = copy_across(&part, &to, &sha256).unwrap_err();
        assert!(
            matches!(&err, DownloadError::Taken(path) if *path == to),
            "{err}"
        );
        assert_eq!(fs::read(&to).unwrap(), NEWER_ZIP);
        assert!(!copying.exists());
        drop(file);
        assert!(!part.exists());

        // The part changed on disk after it was hashed on the way.
        fs::remove_file(&to).unwrap();
        fs::write(&part, b"changed after it arrived").unwrap();
        let err = copy_across(&part, &to, &sha256).unwrap_err();
        assert!(matches!(err, DownloadError::Mismatch), "{err}");
        assert!(!to.exists());
        assert!(!copying.exists());
    }

    #[test]
    fn copying_files_are_left_alone() {
        let folder = Folder::new("copying-taken");
        let part = part_path(&folder.data());
        fs::write(&part, NEWER_ZIP).unwrap();
        let to = folder.downloads().join(&newer().zip);
        let copying = to.with_extension("zip.copying");
        let other = to.with_extension("zip.copying.1");
        for path in [&copying, &other] {
            fs::write(path, b"someone else's file").unwrap();
        }
        copy_across(&part, &to, &newer().sha256).unwrap();
        assert_eq!(fs::read(&to).unwrap(), NEWER_ZIP);
        for path in [&copying, &other] {
            assert_eq!(fs::read(path).unwrap(), b"someone else's file");
        }
        assert_eq!(fs::read_dir(folder.downloads()).unwrap().count(), 3);
    }

    #[test]
    fn a_staging_name_another_copy_holds_is_numbered_past() {
        let folder = Folder::new("copying-live");
        let part = part_path(&folder.data());
        fs::write(&part, NEWER_ZIP).unwrap();
        let to = folder.downloads().join(&newer().zip);
        let copying = to.with_extension("zip.copying");
        let live = Pending::create(&copying, true).unwrap();
        copy_across(&part, &to, &newer().sha256).unwrap();
        assert_eq!(fs::read(&to).unwrap(), NEWER_ZIP);
        drop(live);
        assert!(!copying.exists());
        assert_eq!(fs::read_dir(folder.downloads()).unwrap().count(), 1);
    }

    #[test]
    fn staging_names_run_out() {
        let folder = Folder::new("copying-full");
        let part = part_path(&folder.data());
        fs::write(&part, NEWER_ZIP).unwrap();
        let to = folder.downloads().join(&newer().zip);
        fs::write(to.with_extension("zip.copying"), b"someone else's file").unwrap();
        for number in 1..COPYING_NAMES {
            let name = format!("zip.copying.{number}");
            fs::write(to.with_extension(name), b"someone else's file").unwrap();
        }
        let err = copy_across(&part, &to, &newer().sha256).unwrap_err();
        assert!(
            matches!(&err, DownloadError::Move { source, .. }
                if source.kind() == io::ErrorKind::AlreadyExists),
            "{err}"
        );
        assert!(!to.exists());
        assert_eq!(
            fs::read_dir(folder.downloads()).unwrap().count(),
            COPYING_NAMES as usize
        );
    }

    #[test]
    fn a_missing_source_leaves_copying_files_alone() {
        let folder = Folder::new("copying-no-source");
        let to = folder.downloads().join(&newer().zip);
        let copying = to.with_extension("zip.copying");
        fs::write(&copying, b"someone else's file").unwrap();
        let err = copy_across(&part_path(&folder.data()), &to, &newer().sha256).unwrap_err();
        assert!(matches!(err, DownloadError::Part { .. }), "{err}");
        assert_eq!(fs::read(&copying).unwrap(), b"someone else's file");
        assert!(!to.exists());
        assert_eq!(fs::read_dir(folder.downloads()).unwrap().count(), 1);
    }

    #[test]
    fn missing_downloads_folder() {
        let folder = Folder::new("no-folder");
        let (part, file, digest) = arrived(&folder, NEWER_ZIP);
        let missing = folder.0.join("no such folder");
        let err = keep(&part, file, digest, &newer(), &missing).unwrap_err();
        assert!(matches!(err, DownloadError::Move { .. }), "{err}");
        assert!(!part.exists());
    }

    #[test]
    fn windows_says_where_downloads_is() {
        let downloads = downloads_folder().unwrap();
        assert!(downloads.is_absolute(), "{}", downloads.display());
    }
}
