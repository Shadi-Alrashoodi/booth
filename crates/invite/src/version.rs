use std::fmt;

// The number both ends of a room must share. It goes up whenever anything on the wire changes
// that an older build would read wrong: an invite's fields, the handshake, a message. Builds
// with the same number work together whatever their Booth versions say. It lives in this crate
// because the invite is the lowest thing that carries it; the room's Hello carries the same two.
pub const PROTOCOL: u16 = 1;

pub const VERSION: Version = Version {
    major: number(env!("CARGO_PKG_VERSION_MAJOR")),
    minor: number(env!("CARGO_PKG_VERSION_MINOR")),
    patch: number(env!("CARGO_PKG_VERSION_PATCH")),
};

// Where a friend gets another version, for the room's own sentences and for the app's update
// check, which uses this same constant. tools\release.ps1 reads this line for the address in
// latest.txt.
pub const RELEASES_PAGE: &str = "https://github.com/Shadi-Alrashoodi/booth/releases";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

// How a sentence names the other side's version and this one's: by the Booth version, unless
// the two builds share it and differ only in protocol, which happens between test builds.
pub fn two_versions(theirs: Version, their_protocol: u16) -> (String, String) {
    if theirs == VERSION {
        (
            format!("{theirs} (protocol {their_protocol})"),
            format!("{VERSION} (protocol {PROTOCOL})"),
        )
    } else {
        (theirs.to_string(), VERSION.to_string())
    }
}

const fn number(text: &str) -> u16 {
    let digits = text.as_bytes();
    assert!(!digits.is_empty(), "an empty part in the workspace version");
    let mut value: u16 = 0;
    let mut i = 0;
    while i < digits.len() {
        assert!(
            digits[i].is_ascii_digit(),
            "the workspace version must be three plain numbers"
        );
        value = value * 10 + (digits[i] - b'0') as u16;
        i += 1;
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_build_is_the_workspace_version() {
        assert_eq!(VERSION.to_string(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn versions_order_by_number_not_text() {
        let v = |major, minor, patch| Version {
            major,
            minor,
            patch,
        };
        assert!(v(0, 10, 0) > v(0, 9, 0));
        assert!(v(1, 0, 0) > v(0, 99, 99));
        assert!(v(0, 1, 2) > v(0, 1, 1));
    }

    #[test]
    fn same_version_other_protocol_names_the_protocols() {
        let other = Version {
            major: VERSION.major + 1,
            ..VERSION
        };
        assert_eq!(
            two_versions(other, PROTOCOL + 1),
            (other.to_string(), VERSION.to_string())
        );
        assert_eq!(
            two_versions(VERSION, PROTOCOL + 1),
            (
                format!("{VERSION} (protocol {})", PROTOCOL + 1),
                format!("{VERSION} (protocol {PROTOCOL})")
            )
        );
    }
}
