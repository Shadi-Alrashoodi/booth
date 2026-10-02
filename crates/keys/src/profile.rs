use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::KeyError;

const MAX_PROFILE_LEN: usize = 32;

// Names Windows treats as devices in any folder, whatever the extension.
// "profiles\NUL" would not be a folder we can keep a key in.
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// `%LOCALAPPDATA%\Booth`, or `%LOCALAPPDATA%\Booth\profiles\<name>` so two
/// copies can run side by side on one PC for testing. Local, not roaming:
/// the identity belongs to this device and must not follow the user to
/// another PC through a domain profile.
pub fn data_dir(profile: Option<&str>) -> Result<PathBuf, KeyError> {
    let base = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or(KeyError::NoLocalAppData)?;
    dir_under(&base, profile)
}

fn dir_under(base: &Path, profile: Option<&str>) -> Result<PathBuf, KeyError> {
    let mut dir = base.join("Booth");
    if let Some(name) = profile {
        check_profile_name(name)?;
        dir.push("profiles");
        dir.push(name);
    }
    fs::create_dir_all(&dir).map_err(|source| KeyError::CreateDir {
        path: dir.clone(),
        source,
    })?;
    Ok(dir)
}

fn check_profile_name(name: &str) -> Result<(), KeyError> {
    let allowed = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    if name.is_empty() || name.len() > MAX_PROFILE_LEN || !name.chars().all(allowed) {
        return Err(KeyError::BadProfileName {
            name: name.to_owned(),
        });
    }
    if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(name)) {
        return Err(KeyError::ReservedProfileName {
            name: name.to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_names() {
        for name in ["a", "alice", "Tom_2", "test-host", "0", &"x".repeat(32)] {
            assert!(check_profile_name(name).is_ok(), "{name:?}");
        }
    }

    #[test]
    fn rejects_names_outside_the_pattern() {
        for name in [
            "",
            &"x".repeat(33),
            "two words",
            "..",
            r"..\..\Windows",
            "a/b",
            r"a\b",
            "c:",
            "dot.name",
            "tab\t",
            "caf\u{e9}",
            "\u{0661}",
            "nul\0",
        ] {
            assert!(
                matches!(
                    check_profile_name(name),
                    Err(KeyError::BadProfileName { .. })
                ),
                "{name:?}"
            );
        }
    }

    #[test]
    fn rejects_device_names() {
        for name in ["CON", "con", "Nul", "com1", "LPT9"] {
            assert!(
                matches!(
                    check_profile_name(name),
                    Err(KeyError::ReservedProfileName { .. })
                ),
                "{name:?}"
            );
        }
        assert!(check_profile_name("console").is_ok());
        assert!(check_profile_name("com10").is_ok());
    }

    #[test]
    fn error_message_names_the_rule() {
        let err = check_profile_name("my profile").unwrap_err().to_string();
        assert_eq!(
            err,
            "profile name \"my profile\" is not allowed: use 1 to 32 characters from A-Z, a-z, 0-9, hyphen and underscore"
        );
    }

    #[test]
    fn builds_and_creates_the_folders() {
        let base = env::temp_dir().join(format!("booth-keys-profile-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);

        let default = dir_under(&base, None).expect("default profile");
        assert_eq!(default, base.join("Booth"));
        assert!(default.is_dir());

        let named = dir_under(&base, Some("second")).expect("named profile");
        assert_eq!(named, base.join("Booth").join("profiles").join("second"));
        assert!(named.is_dir());

        assert!(dir_under(&base, Some("..")).is_err());

        fs::remove_dir_all(&base).expect("remove temp dir");
    }
}
