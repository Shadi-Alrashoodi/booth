use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

use zeroize::Zeroizing;

use crate::dpapi;
use crate::error::{FileKind, KeyError};
use crate::movefile::{self, IfExists};

const MAGIC: &[u8; 8] = b"BOOTHKEY";
const VERSION: u8 = 1;

// A DPAPI blob for a few hundred bytes of secret is well under 1 KB. Anything
// near this size is not ours, and reading it all would only waste memory.
// The writer checks the same limit: nothing it saves can be refused later.
pub(crate) const MAX_FILE_LEN: usize = 1 << 20;

/// Encrypts `data` for the current Windows user and replaces `path` with it.
/// A crash or power cut leaves the old file or the new one, never a mix.
/// Writers racing on one file each get a whole file in, and the last to
/// finish wins. A caller that reads, changes and writes back must not run two
/// of those at once.
pub fn write_protected(path: &Path, data: &[u8], description: &str) -> Result<(), KeyError> {
    let file = FileKind::Protected;
    let contents = encode(path, data, description, file)?;
    let tmp = write_tmp(path, &contents, file)?;
    move_into_place(&tmp, path, IfExists::Replace).map_err(|e| move_error(file, &tmp, path, e))
}

pub fn read_protected(path: &Path) -> Result<Zeroizing<Vec<u8>>, KeyError> {
    read(path, FileKind::Protected)
}

// Past this many a list keeps breaking for some other reason, and more
// copies of it help nobody.
const ASIDE_NAMES: u32 = 9;

/// Renames a file Booth could not use to `<name>.bad`, or `<name>.bad2` and
/// on up to `<name>.bad9` when that is taken, and returns where it went. An
/// older one is never replaced, so nothing someone may want back is lost.
pub fn put_aside(path: &Path) -> io::Result<PathBuf> {
    for n in 1..=ASIDE_NAMES {
        let mut name = path.as_os_str().to_owned();
        name.push(if n == 1 {
            String::from(".bad")
        } else {
            format!(".bad{n}")
        });
        let aside = PathBuf::from(name);
        match movefile::move_file(path, &aside, IfExists::Fail) {
            Ok(()) => return Ok(aside),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    // The caller names the file; this says only what stopped it.
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(".bad to .bad{ASIDE_NAMES} are all taken"),
    ))
}

pub(crate) enum NewFile {
    Written,
    AlreadyExists,
}

// Like write_protected, but never replaces a file that is already there, even
// one that appeared a moment ago.
pub(crate) fn write_new(
    path: &Path,
    data: &[u8],
    description: &str,
    file: FileKind,
) -> Result<NewFile, KeyError> {
    let contents = encode(path, data, description, file)?;
    let tmp = write_tmp(path, &contents, file)?;
    match move_into_place(&tmp, path, IfExists::Fail) {
        Ok(()) => Ok(NewFile::Written),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(NewFile::AlreadyExists),
        Err(e) => Err(move_error(file, &tmp, path, e)),
    }
}

pub(crate) fn read(path: &Path, file: FileKind) -> Result<Zeroizing<Vec<u8>>, KeyError> {
    let read_error = |source| KeyError::Read {
        file,
        path: path.to_path_buf(),
        source,
    };
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(read_error)?
        .take(MAX_FILE_LEN as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(read_error)?;

    let blob = match parse(&bytes) {
        Ok(blob) => blob,
        Err(Malformed::NotOurs) => {
            return Err(KeyError::NotAKeyFile {
                file,
                path: path.to_path_buf(),
            });
        }
        Err(Malformed::Version(version)) => {
            return Err(KeyError::UnknownVersion {
                file,
                path: path.to_path_buf(),
                version,
            });
        }
    };

    dpapi::unprotect(blob).map_err(|source| KeyError::Decrypt {
        file,
        path: path.to_path_buf(),
        source,
    })
}

fn encode(
    path: &Path,
    data: &[u8],
    description: &str,
    file: FileKind,
) -> Result<Vec<u8>, KeyError> {
    let blob = dpapi::protect(data, description).map_err(|source| KeyError::Encrypt {
        file,
        path: path.to_path_buf(),
        source,
    })?;

    let len = MAGIC.len() + 1 + blob.len();
    if len > MAX_FILE_LEN {
        return Err(KeyError::TooLarge {
            file,
            path: path.to_path_buf(),
            len,
        });
    }
    let mut contents = Vec::with_capacity(len);
    contents.extend_from_slice(MAGIC);
    contents.push(VERSION);
    contents.extend_from_slice(&blob);
    Ok(contents)
}

enum Malformed {
    NotOurs,
    Version(u8),
}

fn parse(bytes: &[u8]) -> Result<&[u8], Malformed> {
    if bytes.len() > MAX_FILE_LEN {
        return Err(Malformed::NotOurs);
    }
    let rest = bytes
        .strip_prefix(MAGIC.as_slice())
        .ok_or(Malformed::NotOurs)?;
    let (&version, blob) = rest.split_first().ok_or(Malformed::NotOurs)?;
    if version != VERSION {
        return Err(Malformed::Version(version));
    }
    if blob.is_empty() {
        return Err(Malformed::NotOurs);
    }
    Ok(blob)
}

// The data goes to a temp file next to the target first and is renamed over
// it, so the target only ever holds a whole file. On failure the error
// carries the path that actually failed.
fn write_tmp(path: &Path, contents: &[u8], file: FileKind) -> Result<PathBuf, KeyError> {
    let tmp = tmp_path(path);
    if let Err(source) = write_synced(&tmp, contents) {
        let _ = fs::remove_file(&tmp);
        return Err(KeyError::Write {
            file,
            path: tmp,
            source,
        });
    }
    Ok(tmp)
}

fn write_synced(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut file = File::create(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

fn move_into_place(tmp: &Path, path: &Path, if_exists: IfExists) -> io::Result<()> {
    let result = match movefile::move_file(tmp, path, if_exists) {
        // A plain replace is refused while another handle has the target
        // open, even one that allows deletion, and for a read-only target.
        // std's rename retries those with POSIX rename semantics.
        Err(e) if if_exists == IfExists::Replace && e.kind() == io::ErrorKind::PermissionDenied => {
            fs::rename(tmp, path)
        }
        result => result,
    };
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

fn move_error(file: FileKind, tmp: &Path, path: &Path, source: io::Error) -> KeyError {
    // The temp file was created in the target's folder a moment ago, so
    // NotFound here means the temp file went missing, not the folder.
    let failed = if source.kind() == io::ErrorKind::NotFound {
        tmp
    } else {
        path
    };
    KeyError::Write {
        file,
        path: failed.to_path_buf(),
        source,
    }
}

// Unique per write: two writers sharing one temp name would rename each
// other's bytes into place. The process id keeps copies of Booth apart, the
// counter keeps threads apart.
fn tmp_path(path: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut name = path.as_os_str().to_owned();
    name.push(format!(
        ".{}-{}.tmp",
        process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_header(version: u8, blob: &[u8]) -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        bytes.push(version);
        bytes.extend_from_slice(blob);
        bytes
    }

    #[test]
    fn parse_accepts_current_format() {
        let bytes = with_header(VERSION, &[1, 2, 3]);
        assert!(matches!(parse(&bytes), Ok([1, 2, 3])));
    }

    #[test]
    fn parse_rejects_short_and_foreign_files() {
        for bytes in [
            &b""[..],
            b"BOOT",
            b"BOOTHKEY",
            b"BOOTHKEY\x01",
            b"BOOTHKEZ\x01abc",
            b"boothkey\x01abc",
        ] {
            assert!(matches!(parse(bytes), Err(Malformed::NotOurs)), "{bytes:?}");
        }
    }

    #[test]
    fn parse_rejects_other_versions() {
        for version in [0, 2, 255] {
            let bytes = with_header(version, &[1, 2, 3]);
            assert!(matches!(parse(&bytes), Err(Malformed::Version(v)) if v == version));
        }
    }

    #[test]
    fn parse_rejects_oversized_input() {
        let bytes = with_header(VERSION, &vec![0; MAX_FILE_LEN]);
        assert!(matches!(parse(&bytes), Err(Malformed::NotOurs)));
    }

    #[test]
    fn tmp_path_is_next_to_the_target_and_unique() {
        let target = Path::new(r"C:\x\identity.key");
        let a = tmp_path(target);
        let b = tmp_path(target);
        assert_ne!(a, b);
        for tmp in [a, b] {
            assert_eq!(tmp.parent(), target.parent());
            let name = tmp.file_name().and_then(|n| n.to_str()).expect("name");
            let prefix = format!("identity.key.{}-", process::id());
            assert!(name.starts_with(&prefix), "{name}");
            assert!(name.ends_with(".tmp"), "{name}");
        }
    }
}
