use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::file::MAX_FILE_LEN;

/// Which kind of protected file an error is about, so the message can say
/// "the identity key" and give the one fix that works for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Identity,
    Protected,
}

#[derive(Debug)]
#[non_exhaustive]
pub enum KeyError {
    NoLocalAppData,
    BadProfileName {
        name: String,
    },
    ReservedProfileName {
        name: String,
    },
    CreateDir {
        path: PathBuf,
        source: io::Error,
    },
    Read {
        file: FileKind,
        path: PathBuf,
        source: io::Error,
    },
    /// Wrong magic, cut short, empty, or far too large to be one of ours.
    NotAKeyFile {
        file: FileKind,
        path: PathBuf,
    },
    UnknownVersion {
        file: FileKind,
        path: PathBuf,
        version: u8,
    },
    Decrypt {
        file: FileKind,
        path: PathBuf,
        source: io::Error,
    },
    /// The identity file decrypted fine but does not hold a 32-byte key.
    WrongKeyLength {
        path: PathBuf,
        len: usize,
    },
    Encrypt {
        file: FileKind,
        path: PathBuf,
        source: io::Error,
    },
    /// The encrypted file would be larger than the reader accepts. Nothing
    /// was written, so the old file is still there.
    TooLarge {
        file: FileKind,
        path: PathBuf,
        len: usize,
    },
    Write {
        file: FileKind,
        path: PathBuf,
        source: io::Error,
    },
}

// The OS error is part of the message rather than returned from source(), so
// the one line the app shows already says what Windows reported.
impl std::error::Error for KeyError {}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // LOCALAPPDATA is only missing when Booth runs outside a normal
            // sign-in: as a service, a scheduled task, or from a script that
            // started it with an empty environment.
            KeyError::NoLocalAppData => write!(
                f,
                "could not find the local app data folder: LOCALAPPDATA is not set to a full path. Start Booth from your own Windows account, not as a service or from a script that clears the environment."
            ),
            KeyError::BadProfileName { name } => write!(
                f,
                "profile name {name:?} is not allowed: use 1 to 32 characters from A-Z, a-z, 0-9, hyphen and underscore"
            ),
            KeyError::ReservedProfileName { name } => write!(
                f,
                "profile name {name:?} is not allowed: Windows reserves it for a device, pick another"
            ),
            KeyError::CreateDir { path, source } => {
                write!(
                    f,
                    "could not create the folder {}: {source}",
                    path.display()
                )
            }
            KeyError::Read { file, path, source } => {
                write!(f, "could not read {}: {source}", describe(*file, path))
            }
            KeyError::NotAKeyFile { file, path } => write!(
                f,
                "could not read {}: it is not a Booth key file, or it is damaged.{}",
                describe(*file, path),
                advice(*file)
            ),
            KeyError::UnknownVersion {
                file,
                path,
                version,
            } => write!(
                f,
                "could not read {}: it uses file format {version}, which this version of Booth does not know. It was probably written by a newer Booth. Run the newest version of Booth.",
                describe(*file, path)
            ),
            KeyError::Decrypt { file, path, source } => write!(
                f,
                "could not read {}: Windows could not decrypt it. It was made by another Windows user or on another PC, or the file is damaged.{} Windows said: {source}",
                describe(*file, path),
                advice(*file)
            ),
            KeyError::WrongKeyLength { path, len } => write!(
                f,
                "could not read {}: it holds {len} bytes instead of 32.{}",
                describe(FileKind::Identity, path),
                advice(FileKind::Identity)
            ),
            KeyError::Encrypt { file, path, source } => write!(
                f,
                "could not save {}: Windows could not encrypt it. Windows said: {source}",
                describe(*file, path)
            ),
            KeyError::TooLarge { file, path, len } => write!(
                f,
                "could not save {}: it would be {len} bytes, and a Booth key file can hold at most {MAX_FILE_LEN}. The file on disk was left as it was.",
                describe(*file, path)
            ),
            KeyError::Write { file, path, source } => {
                write!(f, "could not save {}: {source}", describe(*file, path))
            }
        }
    }
}

fn describe(file: FileKind, path: &Path) -> String {
    match file {
        FileKind::Identity => format!("the identity key at {}", path.display()),
        FileKind::Protected => path.display().to_string(),
    }
}

// Protected files hold per-device secrets and the known devices list, and
// what either one covers comes back through a new invite.
fn advice(file: FileKind) -> &'static str {
    match file {
        FileKind::Identity => {
            " Delete the file to make a new identity. Hosts that knew this PC will need to invite it again."
        }
        FileKind::Protected => " Delete the file. Any device it was for will need a new invite.",
    }
}
