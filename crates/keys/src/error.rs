use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::file::{MAX_FILE_LEN, tmp_target};

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
        self.words(f, true)
    }
}

impl KeyError {
    // The same words naming each file but not its folder, which is under
    // the Windows profile, for lines on screen; Display, for the log, keeps
    // the whole path.
    pub fn without_folders(&self) -> String {
        let mut text = String::new();
        let _ = self.words(&mut text, false);
        text
    }

    fn words(&self, f: &mut dyn fmt::Write, folders: bool) -> fmt::Result {
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
            KeyError::CreateDir { path, source } if folders => {
                write!(
                    f,
                    "could not create the folder {}: {source}",
                    path.display()
                )
            }
            KeyError::CreateDir { source, .. } => {
                write!(f, "could not create Booth's data folder: {source}")
            }
            KeyError::Read { file, path, source } => write!(
                f,
                "could not read {}: {source}",
                describe(*file, path, folders)
            ),
            KeyError::NotAKeyFile { file, path } => write!(
                f,
                "could not read {}: it is not a Booth key file, or it is damaged.{}",
                describe(*file, path, folders),
                advice(*file)
            ),
            KeyError::UnknownVersion {
                file,
                path,
                version,
            } => write!(
                f,
                "could not read {}: it uses file format {version}, which this version of Booth does not know. It was probably written by a newer Booth. Run the newest version of Booth.",
                describe(*file, path, folders)
            ),
            KeyError::Decrypt { file, path, source } => write!(
                f,
                "could not read {}: Windows could not decrypt it. It was made by another Windows user or on another PC, or the file is damaged.{} Windows said: {source}",
                describe(*file, path, folders),
                advice(*file)
            ),
            KeyError::WrongKeyLength { path, len } => write!(
                f,
                "could not read {}: it holds {len} bytes instead of 32.{}",
                describe(FileKind::Identity, path, folders),
                advice(FileKind::Identity)
            ),
            KeyError::Encrypt { file, path, source } => write!(
                f,
                "could not save {}: Windows could not encrypt it. Windows said: {source}",
                describe(*file, path, folders)
            ),
            KeyError::TooLarge { file, path, len } => write!(
                f,
                "could not save {}: it would be {len} bytes, and a Booth key file can hold at most {MAX_FILE_LEN}. The file on disk was left as it was.",
                describe(*file, path, folders)
            ),
            KeyError::Write { file, path, source } => write!(
                f,
                "could not save {}: {source}",
                describe(*file, path, folders)
            ),
        }
    }
}

fn describe(file: FileKind, path: &Path, folders: bool) -> String {
    if folders {
        return match file {
            FileKind::Identity => format!("the identity key at {}", path.display()),
            FileKind::Protected => path.display().to_string(),
        };
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let name = tmp_target(&name).unwrap_or(&name);
    match file {
        FileKind::Identity => format!("the identity key in {name}"),
        FileKind::Protected => name.to_owned(),
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

#[cfg(test)]
mod tests {
    use super::*;

    // On screen each file is named without the folder under the Windows
    // profile, and a temporary copy by the file it was for; the log keeps
    // the whole path.
    #[test]
    fn words_without_folders() {
        let dir = Path::new(r"C:\Users\Mara\AppData\Local\Booth");
        let denied = || io::Error::from_raw_os_error(5);
        let errors = [
            KeyError::Read {
                file: FileKind::Protected,
                path: dir.join("devices.bin"),
                source: denied(),
            },
            KeyError::Write {
                file: FileKind::Protected,
                path: dir.join("devices.bin.4242-0.tmp"),
                source: io::Error::from_raw_os_error(112),
            },
            KeyError::Write {
                file: FileKind::Protected,
                path: dir.join("devices.bin"),
                source: denied(),
            },
            KeyError::TooLarge {
                file: FileKind::Protected,
                path: dir.join("devices.bin"),
                len: MAX_FILE_LEN + 1,
            },
        ];
        for err in &errors {
            let log = err.to_string();
            assert!(log.contains("Mara"), "{log}");
            let shown = err.without_folders();
            assert!(!shown.contains("Mara"), "{shown}");
            assert!(!shown.contains('\\'), "{shown}");
            assert!(!shown.contains(".tmp"), "{shown}");
            assert!(shown.contains(" devices.bin: "), "{shown}");
        }
    }
}
