// latest.txt as the release script writes it, one `key = value` per line:
// version, published, zip, sha256 and url, then installer and
// installer_sha256 when the release has an installer. It comes off the
// internet, so it is read as hostile even though it is signed: every byte
// and every line is checked, a key this version does not know is refused
// rather than skipped, and the zip's and the installer's names are fixed by
// the version so they can never name a path. The check downloads only the
// zip; the installer's lines are there for checking a download by hand.

use std::fmt;
use std::str::FromStr;

use minisign_verify::{Error as MinisignError, PublicKey, Signature};

// A manifest is about 350 bytes and its signature about 300. Anything much
// bigger is not one, and is not read any further than this.
pub const MOST_MANIFEST_BYTES: usize = 4096;
pub const MOST_SIGNATURE_BYTES: usize = 2048;
const MOST_URL_CHARS: usize = 1024;

const VERSION: &str = "version";
const PUBLISHED: &str = "published";
const ZIP: &str = "zip";
const SHA256: &str = "sha256";
const URL: &str = "url";
const INSTALLER: &str = "installer";
const INSTALLER_SHA256: &str = "installer_sha256";
const KEYS: [&str; 7] = [
    VERSION,
    PUBLISHED,
    ZIP,
    SHA256,
    URL,
    INSTALLER,
    INSTALLER_SHA256,
];

// Three numbers compared as numbers, in this order, so 0.10.0 is newer than
// 0.9.0. No suffixes: a release that is not ready is not put behind latest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    major: u32,
    minor: u32,
    patch: u32,
}

impl Version {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Version {
        Version {
            major,
            minor,
            patch,
        }
    }

    // The invite crate reads the workspace version when it is built, so a
    // version that is not three numbers stops the build, not Booth.
    pub const fn running() -> Version {
        let own = invite::VERSION;
        Version::new(own.major as u32, own.minor as u32, own.patch as u32)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl FromStr for Version {
    type Err = ();

    fn from_str(text: &str) -> Result<Version, ()> {
        let mut parts = text.split('.');
        let mut number = || parts.next().and_then(version_part).ok_or(());
        let version = Version::new(number()?, number()?, number()?);
        if parts.next().is_some() {
            return Err(());
        }
        Ok(version)
    }
}

// Digits only, no leading zero unless it is the whole part, and at most nine
// of them, which always fits a u32.
fn version_part(text: &str) -> Option<u32> {
    let digits = !text.is_empty() && text.len() <= 9 && text.bytes().all(|b| b.is_ascii_digit());
    if !digits || (text.len() > 1 && text.starts_with('0')) {
        return None;
    }
    text.parse().ok()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub version: Version,
    // As written, YYYY-MM-DD, checked to be a real date.
    pub published: String,
    // Always booth-{version}-windows-x64.zip.
    pub zip: String,
    pub sha256: [u8; 32],
    // https, no port, no user name, and a path that ends in the zip's name.
    pub url: String,
    pub installer: Option<Installer>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installer {
    // Always booth-{version}-setup.exe.
    pub name: String,
    pub sha256: [u8; 32],
}

impl Manifest {
    pub fn sha256_hex(&self) -> String {
        hex(&self.sha256)
    }
}

impl Installer {
    pub fn sha256_hex(&self) -> String {
        hex(&self.sha256)
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn zip_name(version: Version) -> String {
    format!("booth-{version}-windows-x64.zip")
}

pub fn installer_name(version: Version) -> String {
    format!("booth-{version}-setup.exe")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    Empty,
    TooLong,
    // The line, counted from 1.
    NotPlainText(usize),
    Blank(usize),
    NoEquals(usize),
    Unknown { line: usize, key: String },
    Twice { line: usize, key: &'static str },
    Missing(&'static str),
    Value { line: usize, key: &'static str },
    ZipName,
    InstallerName,
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::Empty => write!(f, "latest.txt is empty"),
            Refused::TooLong => write!(
                f,
                "latest.txt is longer than the {MOST_MANIFEST_BYTES} bytes a manifest can be"
            ),
            Refused::NotPlainText(line) => write!(
                f,
                "latest.txt line {line} holds something other than plain ASCII text"
            ),
            Refused::Blank(line) => write!(f, "latest.txt line {line} is blank"),
            Refused::NoEquals(line) => write!(f, "latest.txt line {line} has no ="),
            Refused::Unknown { line, key } => write!(
                f,
                "latest.txt line {line} has {key:?}, which is not a key this version knows"
            ),
            Refused::Twice { line, key } => {
                write!(f, "latest.txt line {line} gives {key} a second time")
            }
            Refused::Missing(key) => write!(f, "latest.txt has no {key}"),
            Refused::Value { line, key } => {
                write!(f, "latest.txt line {line} has a {key} Booth cannot use")
            }
            Refused::ZipName => write!(
                f,
                "the zip named in latest.txt is not the one for its version"
            ),
            Refused::InstallerName => write!(
                f,
                "the installer named in latest.txt is not the one for its version"
            ),
        }
    }
}

pub fn parse(bytes: &[u8]) -> Result<Manifest, Refused> {
    if bytes.is_empty() {
        return Err(Refused::Empty);
    }
    if bytes.len() > MOST_MANIFEST_BYTES {
        return Err(Refused::TooLong);
    }
    // One line break at the very end is how files end; a second would be a
    // blank line and is refused below.
    let body = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let mut found: [Option<(usize, &str)>; KEYS.len()] = [None; KEYS.len()];
    for (index, line) in body.split(|&byte| byte == b'\n').enumerate() {
        let number = index + 1;
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if !line.iter().all(|&byte| (0x20..=0x7e).contains(&byte)) {
            return Err(Refused::NotPlainText(number));
        }
        // Plain ASCII was just checked, so this cannot fail.
        let line = std::str::from_utf8(line).map_err(|_| Refused::NotPlainText(number))?;
        if line.trim_matches(' ').is_empty() {
            return Err(Refused::Blank(number));
        }
        let (key, value) = line.split_once('=').ok_or(Refused::NoEquals(number))?;
        let key = key.trim_matches(' ');
        let Some(slot) = KEYS.iter().position(|known| *known == key) else {
            return Err(Refused::Unknown {
                line: number,
                key: key.to_owned(),
            });
        };
        if found[slot].is_some() {
            return Err(Refused::Twice {
                line: number,
                key: KEYS[slot],
            });
        }
        found[slot] = Some((number, value.trim_matches(' ')));
    }
    let take = |slot: usize| found[slot].ok_or(Refused::Missing(KEYS[slot]));
    let (version, published, zip, sha256, url) = (take(0)?, take(1)?, take(2)?, take(3)?, take(4)?);
    let refused = |(line, _): (usize, &str), key| Refused::Value { line, key };

    let parsed_version = version.1.parse().map_err(|_| refused(version, VERSION))?;
    if !date(published.1) {
        return Err(refused(published, PUBLISHED));
    }
    if zip.1 != zip_name(parsed_version) {
        return Err(Refused::ZipName);
    }
    let digest = sha256_value(sha256.1).ok_or_else(|| refused(sha256, SHA256))?;
    if !download_url(url.1, zip.1) {
        return Err(refused(url, URL));
    }
    // Both lines or neither. A release built without Inno Setup has none,
    // and so do the signed manifests in the tests, whose key's secret half
    // is gone, so they cannot be signed again with the lines added.
    let installer = match (found[5], found[6]) {
        (None, None) => None,
        (Some(_), None) => return Err(Refused::Missing(INSTALLER_SHA256)),
        (None, Some(_)) => return Err(Refused::Missing(INSTALLER)),
        (Some(name), Some(hash)) => {
            if name.1 != installer_name(parsed_version) {
                return Err(Refused::InstallerName);
            }
            let sha256 = sha256_value(hash.1).ok_or_else(|| refused(hash, INSTALLER_SHA256))?;
            Some(Installer {
                name: name.1.to_owned(),
                sha256,
            })
        }
    };
    Ok(Manifest {
        version: parsed_version,
        published: published.1.to_owned(),
        zip: zip.1.to_owned(),
        sha256: digest,
        url: url.1.to_owned(),
        installer,
    })
}

// YYYY-MM-DD, a day that exists.
fn date(text: &str) -> bool {
    let bytes = text.as_bytes();
    let shape = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, byte)| i == 4 || i == 7 || byte.is_ascii_digit());
    if !shape {
        return false;
    }
    let number = |range: std::ops::Range<usize>| text[range].parse::<u32>().unwrap_or(0);
    let (year, month, day) = (number(0..4), number(5..7), number(8..10));
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    year >= 2000 && (1..=days).contains(&day)
}

// 64 lowercase hex digits, as the release script writes them.
fn sha256_value(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let digit = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; 32];
    for (i, pair) in bytes.chunks(2).enumerate() {
        out[i] = (digit(pair[0])? << 4) | digit(pair[1])?;
    }
    Some(out)
}

// https on the default port to a plain host name, and a path of plain
// characters that ends in the zip's own name: no user name, no query, no
// escapes and no dot segments, so the address says exactly what it looks
// like it says.
fn download_url(text: &str, zip: &str) -> bool {
    if text.len() > MOST_URL_CHARS {
        return false;
    }
    let Some(rest) = text.strip_prefix("https://") else {
        return false;
    };
    let Some((host, path)) = rest.split_once('/') else {
        return false;
    };
    let host_ok = !host.is_empty()
        && host.len() <= 253
        && host
            .split('.')
            .all(|label| !label.is_empty() && label.len() <= 63 && host_label(label));
    let plain = |byte: u8| byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte);
    let path_ok = path.bytes().all(plain)
        && path
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..");
    let ends_in_zip = path == zip || path.ends_with(&format!("/{zip}"));
    host_ok && path_ok && ends_in_zip
}

fn host_label(label: &str) -> bool {
    let bytes = label.as_bytes();
    bytes
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
        && bytes[0] != b'-'
        && bytes[bytes.len() - 1] != b'-'
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignatureRefused {
    // The key compiled into this copy is not a minisign public key, which
    // is how a build without the release key looks.
    NoKey,
    TooLong,
    Unreadable,
    // Made with some other key.
    OtherKey,
    // Made by the release key, but over other bytes, or forged.
    Wrong,
    // Minisign's old format, which signs the file itself rather than its
    // hash; the release tool never makes one.
    Legacy,
}

impl fmt::Display for SignatureRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SignatureRefused::NoKey => {
                "this copy of Booth was built without the release key, so it cannot check a release"
            }
            SignatureRefused::TooLong => {
                "latest.txt.minisig is longer than a minisign signature can be"
            }
            SignatureRefused::Unreadable => "latest.txt.minisig is not a minisign signature",
            SignatureRefused::OtherKey => "latest.txt was signed with a key that is not Booth's",
            SignatureRefused::Wrong => "latest.txt does not match its signature",
            SignatureRefused::Legacy => {
                "latest.txt is signed in minisign's old format, which Booth does not take"
            }
        })
    }
}

// Asked before anything is fetched: a copy built without the release key
// has nothing to check a release with.
pub fn usable_key(key: &str) -> bool {
    PublicKey::from_base64(key).is_ok()
}

// The signature is checked over the bytes exactly as they came, before any
// of them is read as a manifest.
pub fn verify(manifest: &[u8], signature: &[u8], key: &str) -> Result<(), SignatureRefused> {
    let key = PublicKey::from_base64(key).map_err(|_| SignatureRefused::NoKey)?;
    if signature.len() > MOST_SIGNATURE_BYTES {
        return Err(SignatureRefused::TooLong);
    }
    let text = std::str::from_utf8(signature).map_err(|_| SignatureRefused::Unreadable)?;
    let signature = Signature::decode(text).map_err(|_| SignatureRefused::Unreadable)?;
    key.verify(manifest, &signature, false)
        .map_err(|err| match err {
            MinisignError::UnexpectedKeyId => SignatureRefused::OtherKey,
            MinisignError::InvalidSignature => SignatureRefused::Wrong,
            MinisignError::UnexpectedAlgorithm => SignatureRefused::Legacy,
            _ => SignatureRefused::Unreadable,
        })
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::cmp::Ordering;

    // Made once for these tests, with the minisign crate the release tool
    // uses, from a throwaway key pair whose secret half was never written
    // down. The untrusted comments were changed by hand afterwards, which is
    // allowed: minisign does not sign them. Their addresses name a made-up
    // account, OWNER, and cannot change without signing them again.
    pub const TEST_KEY: &str = "RWSssjXmIDqlYOgyOyMhBTzD0wS7BkCGc4w1WFD0JYvKS9PILgMOgQez";

    pub const NEWER: &str = "version = 0.2.0\npublished = 2026-10-14\nzip = booth-0.2.0-windows-x64.zip\nsha256 = 9370b493e8b8dd9bfab868ac564fc7130f4d0033a96195489e7e8ed393faaf84\nurl = https://github.com/OWNER/booth/releases/download/v0.2.0/booth-0.2.0-windows-x64.zip\n";
    pub const NEWER_SIGNED: &str = "untrusted comment: signature from the throwaway test key
RUSssjXmIDqlYGf+6PchsJy/qmzlb4FJN/m07/c+Zu0T6Hd+PgYYAhC8sf35eTjvyCdRNQGZYwCkMg84EVsTMaTiQ4+Vm4aLnAQ=
trusted comment: timestamp:1791936000\tfile:latest.txt\thashed
xCnrFo2UY4Su6F7zUd68kqDgp/UOXHp3Txzc4F65NKQoD3DmXYPzbG1XY6Mi7VnL/Kj8Ei0SIraKrV0/Ez32Aw==
";
    // What NEWER's sha256 is the hash of.
    pub const NEWER_ZIP: &[u8] = b"stands in for booth-0.2.0-windows-x64.zip\n";

    pub const SAME: &str = "version = 0.1.0\npublished = 2026-10-01\nzip = booth-0.1.0-windows-x64.zip\nsha256 = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\nurl = https://github.com/OWNER/booth/releases/download/v0.1.0/booth-0.1.0-windows-x64.zip\n";
    pub const SAME_SIGNED: &str = "untrusted comment: signature from the throwaway test key
RUSssjXmIDqlYMxw3Yov3ZIJdlRO7K1B0mUI8KYExB8orTCqkbA7DTTWOFZ4Z62aTMgAeiUEfEwRj3KyFqX5tgMx7cURrinXdwY=
trusted comment: timestamp:1791936000\tfile:latest.txt\thashed
/Uu20wfIe3MPuKo5WaXWxA/uBqxyreCivtk+8k4tubJTmLhVfJ7DkbrIFkqbEXhCqB6s/kU9P5XevzOvyWn6DQ==
";

    // Signed by the same key, with a line no version of Booth knows.
    pub const UNKNOWN_KEY: &str = "version = 0.2.0\npublished = 2026-10-14\nzip = booth-0.2.0-windows-x64.zip\nsha256 = 9370b493e8b8dd9bfab868ac564fc7130f4d0033a96195489e7e8ed393faaf84\nurl = https://github.com/OWNER/booth/releases/download/v0.2.0/booth-0.2.0-windows-x64.zip\nnote = signed, but not a key this version knows\n";
    pub const UNKNOWN_KEY_SIGNED: &str = "untrusted comment: signature from the throwaway test key
RUSssjXmIDqlYFenH/u9t+MM8TdGjPmq40FkQpKn4lArzQTSwx2Dbd6uE99LUNXBFlfGmLa4sflXMAuvm50TsDBBecACGN4MyQ8=
trusted comment: timestamp:1791936000\tfile:latest.txt\thashed
UcWSkeDHK6BJ5xAMZ/NOJafU3NzkDSmF4FogeWbOYqnbr7nKyWjssrTTvbtwum6W48AjBGN9u5qItA6YB5PaCA==
";

    // NEWER signed by a second throwaway key, and the same signature with
    // the first key's id put in its place, so only the signature is wrong.
    const NEWER_BY_OTHER_KEY: &str = "untrusted comment: signature from another throwaway key
RUS/XRELv3sSWdfisaVKqw3Y4gKPg27zdPFa9FIr9LzdMrBtmAaw8N9JdOPEFFn6gtf7ne6iIgQ5X5uL7wclTOWA/i3LddrWGAA=
trusted comment: timestamp:1791936000\tfile:latest.txt\thashed
lGB7Q59E1azRbor/kgPYbQWTCkBe+OhrPm2q3SzzNWDE+CNcnGG7pud3nwdOm26KlwsZBOZciqKtnU6+bf08Bw==
";
    const NEWER_BY_OTHER_KEY_WITH_OUR_ID: &str =
        "untrusted comment: signature from another throwaway key
RUSssjXmIDqlYNfisaVKqw3Y4gKPg27zdPFa9FIr9LzdMrBtmAaw8N9JdOPEFFn6gtf7ne6iIgQ5X5uL7wclTOWA/i3LddrWGAA=
trusted comment: timestamp:1791936000\tfile:latest.txt\thashed
lGB7Q59E1azRbor/kgPYbQWTCkBe+OhrPm2q3SzzNWDE+CNcnGG7pud3nwdOm26KlwsZBOZciqKtnU6+bf08Bw==
";

    // minisign-verify's own test vectors, over the four bytes "test": the
    // current hashed format and the old one.
    const VECTOR_KEY: &str = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
    const VECTOR_HASHED: &str = "untrusted comment: signature from minisign secret key
RUQf6LRCGA9i559r3g7V1qNyJDApGip8MfqcadIgT9CuhV3EMhHoN1mGTkUidF/z7SrlQgXdy8ofjb7bNJJylDOocrCo8KLzZwo=
trusted comment: timestamp:1556193335\tfile:test
y/rUw2y8/hOUYjZU71eHp/Wo1KZ40fGy2VJEDl34XMJM+TX48Ss/17u3IvIfbVR1FkZZSNCisQbuQY+bHwhEBg==";
    const VECTOR_LEGACY: &str = "untrusted comment: signature from minisign secret key
RWQf6LRCGA9i59SLOFxz6NxvASXDJeRtuZykwQepbDEGt87ig1BNpWaVWuNrm73YiIiJbq71Wi+dP9eKL8OC351vwIasSSbXxwA=
trusted comment: timestamp:1555779966\tfile:test
QtKMXWyYcwdpZAlPF7tE2ENJkRd1ujvKjlj1m9RtHTBnZPa5WKU5uWRs5GoP5M/VqE81QFuMKI5k/SfNQUaOAA==";

    fn good(text: &str) -> Manifest {
        parse(text.as_bytes()).unwrap_or_else(|why| panic!("{text:?}: {why}"))
    }

    #[test]
    fn a_manifest_as_the_release_script_writes_it() {
        let manifest = good(NEWER);
        assert_eq!(manifest.version, Version::new(0, 2, 0));
        assert_eq!(manifest.published, "2026-10-14");
        assert_eq!(manifest.zip, "booth-0.2.0-windows-x64.zip");
        assert_eq!(
            manifest.sha256_hex(),
            "9370b493e8b8dd9bfab868ac564fc7130f4d0033a96195489e7e8ed393faaf84"
        );
        assert_eq!(
            manifest.url,
            "https://github.com/OWNER/booth/releases/download/v0.2.0/booth-0.2.0-windows-x64.zip"
        );
        assert_eq!(manifest.installer, None);
    }

    // The SHA-256 of "stands in for booth-0.2.0-setup.exe\n".
    const INSTALLER_HASH: &str = "6cc6aff84513883a0d7069045e41ba801bd4662550cbfd401a6c21a340c989db";

    fn with_installer(name: &str, hash: &str) -> String {
        format!("{NEWER}installer = {name}\ninstaller_sha256 = {hash}\n")
    }

    #[test]
    fn the_installer_lines_are_read_as_a_pair() {
        // As release.ps1 writes them, after the five, with CRLF.
        let text = with_installer("booth-0.2.0-setup.exe", INSTALLER_HASH).replace('\n', "\r\n");
        let manifest = good(&text);
        let installer = manifest.installer.clone().expect("the installer lines");
        assert_eq!(installer.name, "booth-0.2.0-setup.exe");
        assert_eq!(installer.sha256_hex(), INSTALLER_HASH);
        assert_eq!(
            Manifest {
                installer: None,
                ..manifest.clone()
            },
            good(NEWER)
        );
        let mut first: Vec<&str> = text.lines().collect();
        first.rotate_right(2);
        assert_eq!(good(&first.join("\n")), manifest);
    }

    #[test]
    fn half_the_installer_pair_is_refused() {
        let name = "installer = booth-0.2.0-setup.exe\n";
        let hash = format!("installer_sha256 = {INSTALLER_HASH}\n");
        assert_eq!(
            refused(&format!("{NEWER}{name}")),
            Refused::Missing(INSTALLER_SHA256)
        );
        assert_eq!(
            refused(&format!("{NEWER}{hash}")),
            Refused::Missing(INSTALLER)
        );
    }

    #[test]
    fn an_installer_line_given_twice_is_refused() {
        let text = with_installer("booth-0.2.0-setup.exe", INSTALLER_HASH);
        assert_eq!(
            refused(&format!("{text}installer = booth-0.2.0-setup.exe\n")),
            Refused::Twice {
                line: 8,
                key: INSTALLER
            }
        );
        assert_eq!(
            refused(&format!("{text}installer_sha256 = {INSTALLER_HASH}\n")),
            Refused::Twice {
                line: 8,
                key: INSTALLER_SHA256
            }
        );
    }

    #[test]
    fn bad_installer_hash() {
        for hash in [
            "6CC6AFF84513883A0D7069045E41BA801BD4662550CBFD401A6C21A340C989DB",
            "6cc6aff84513883a0d7069045e41ba801bd4662550cbfd401a6c21a340c989d",
            "6cc6aff84513883a0d7069045e41ba801bd4662550cbfd401a6c21a340c989dbaa",
            "gcc6aff84513883a0d7069045e41ba801bd4662550cbfd401a6c21a340c989db",
            "6cc6aff8 4513883a0d7069045e41ba801bd4662550cbfd401a6c21a340c989db",
            "",
        ] {
            assert_eq!(
                refused(&with_installer("booth-0.2.0-setup.exe", hash)),
                Refused::Value {
                    line: 7,
                    key: INSTALLER_SHA256
                },
                "{hash:?}"
            );
        }
    }

    // Like the zip's, the installer's name is never taken from the file.
    #[test]
    fn installer_name_from_version() {
        for name in [
            "booth-0.3.0-setup.exe",
            "booth-0.1.0-setup.exe",
            "..\\booth-0.2.0-setup.exe",
            "../booth-0.2.0-setup.exe",
            "C:\\Windows\\booth-0.2.0-setup.exe",
            "booth-0.2.0-setup.msi",
            "Booth-0.2.0-setup.exe",
            "booth-0.2.0-windows-x64.zip",
            "",
        ] {
            assert_eq!(
                refused(&with_installer(name, INSTALLER_HASH)),
                Refused::InstallerName,
                "{name:?}"
            );
        }
    }

    // The address release.ps1 writes from invite::RELEASES_PAGE must pass
    // these rules, or every copy would refuse the real release's manifest.
    #[test]
    fn real_releases_page_passes() {
        let url = format!(
            "{}/download/v0.1.0/booth-0.1.0-windows-x64.zip",
            invite::RELEASES_PAGE
        );
        let text = SAME.replace(
            "https://github.com/OWNER/booth/releases/download/v0.1.0/booth-0.1.0-windows-x64.zip",
            &url,
        );
        assert_ne!(text, SAME);
        assert_eq!(good(&text).url, url);
    }

    #[test]
    fn line_breaks_spaces_and_order_do_not_matter() {
        let expected = good(NEWER);
        let crlf = NEWER.replace('\n', "\r\n");
        let no_last_break = NEWER.trim_end();
        let tight = NEWER.replace(" = ", "=");
        let mut reordered: Vec<&str> = NEWER.lines().collect();
        reordered.reverse();
        let reordered = reordered.join("\n");
        for text in [crlf.as_str(), no_last_break, tight.as_str(), &reordered] {
            assert_eq!(good(text), expected, "{text:?}");
        }
    }

    fn refused(text: &str) -> Refused {
        match parse(text.as_bytes()) {
            Ok(manifest) => panic!("{text:?} was taken: {manifest:?}"),
            Err(why) => why,
        }
    }

    fn with(key: &str, value: &str) -> String {
        NEWER
            .lines()
            .map(|line| {
                if line.starts_with(&format!("{key} ")) {
                    format!("{key} = {value}")
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn bad_values() {
        let cases: &[(&str, &str, usize)] = &[
            ("version", "0.2", 1),
            ("version", "0.2.0.1", 1),
            ("version", "0.2.0-beta", 1),
            ("version", "v0.2.0", 1),
            ("version", "0.02.0", 1),
            ("version", "0.+2.0", 1),
            ("version", "0.2.4294967296", 1),
            ("version", "", 1),
            ("published", "2026-02-29", 2),
            ("published", "2026-13-01", 2),
            ("published", "2026-1-01", 2),
            ("published", "14.10.2026", 2),
            ("published", "1999-12-31", 2),
            (
                "sha256",
                "9370B493E8B8DD9BFAB868AC564FC7130F4D0033A96195489E7E8ED393FAAF84",
                4,
            ),
            (
                "sha256",
                "9370b493e8b8dd9bfab868ac564fc7130f4d0033a96195489e7e8ed393faaf8",
                4,
            ),
            (
                "sha256",
                "9370b493e8b8dd9bfab868ac564fc7130f4d0033a96195489e7e8ed393faaf84aa",
                4,
            ),
            (
                "sha256",
                "g370b493e8b8dd9bfab868ac564fc7130f4d0033a96195489e7e8ed393faaf84",
                4,
            ),
            (
                "url",
                "http://github.com/OWNER/booth/releases/download/v0.2.0/booth-0.2.0-windows-x64.zip",
                5,
            ),
            (
                "url",
                "https://github.com:8443/OWNER/booth/booth-0.2.0-windows-x64.zip",
                5,
            ),
            (
                "url",
                "https://user@github.com/OWNER/booth/booth-0.2.0-windows-x64.zip",
                5,
            ),
            (
                "url",
                "https://github.com/OWNER/booth/booth-0.2.0-windows-x64.zip?x=1",
                5,
            ),
            (
                "url",
                "https://github.com/OWNER/../booth-0.2.0-windows-x64.zip",
                5,
            ),
            (
                "url",
                "https://github.com/OWNER//booth-0.2.0-windows-x64.zip",
                5,
            ),
            (
                "url",
                "https://github.com/OWNER/booth%2e0.2.0-windows-x64.zip",
                5,
            ),
            (
                "url",
                "https://github.com/OWNER/booth/booth-0.1.0-windows-x64.zip",
                5,
            ),
            (
                "url",
                "https://github.com/OWNER/booth/booth-0.2.0-windows-x64.zip.exe",
                5,
            ),
            ("url", "https://-github.com/booth-0.2.0-windows-x64.zip", 5),
            ("url", "https://github..com/booth-0.2.0-windows-x64.zip", 5),
            ("url", "https:///booth-0.2.0-windows-x64.zip", 5),
            ("url", "https://github.com", 5),
            ("url", "file:///C:/booth-0.2.0-windows-x64.zip", 5),
        ];
        for (key, value, line) in cases {
            let text = with(key, value);
            let why = refused(&text);
            assert!(
                matches!(why, Refused::Value { line: at, key: k } if at == *line && k == *key),
                "{key} = {value}: {why:?}"
            );
        }
        let long = format!(
            "https://github.com/{}/booth-0.2.0-windows-x64.zip",
            "a".repeat(1000)
        );
        assert_eq!(
            refused(&with("url", &long)),
            Refused::Value { line: 5, key: URL }
        );
    }

    // The zip's name is where the download lands in Downloads, so it is
    // never taken from the file as it stands.
    #[test]
    fn zip_name_from_version() {
        for zip in [
            "booth-0.3.0-windows-x64.zip",
            "..\\booth-0.2.0-windows-x64.zip",
            "../booth-0.2.0-windows-x64.zip",
            "C:\\Windows\\booth-0.2.0-windows-x64.zip",
            "booth-0.2.0-windows-x64.exe",
            "Booth-0.2.0-windows-x64.zip",
        ] {
            assert_eq!(refused(&with("zip", zip)), Refused::ZipName, "{zip}");
        }
    }

    #[test]
    fn not_a_manifest() {
        let cases: Vec<(String, Refused)> = vec![
            (String::new(), Refused::Empty),
            ("x".repeat(MOST_MANIFEST_BYTES + 1), Refused::TooLong),
            (format!("\u{feff}{NEWER}"), Refused::NotPlainText(1)),
            (
                NEWER.replace("0.2.0\n", "0.2.0\t\n"),
                Refused::NotPlainText(1),
            ),
            (format!("{NEWER}\n"), Refused::Blank(6)),
            (format!("\n{NEWER}"), Refused::Blank(1)),
            (
                NEWER.replace("\npublished", "\n\npublished"),
                Refused::Blank(2),
            ),
            (format!("{NEWER}# a comment\n"), Refused::NoEquals(6)),
            (
                format!("{NEWER}Version = 0.3.0\n"),
                Refused::Unknown {
                    line: 6,
                    key: String::from("Version"),
                },
            ),
            (
                format!("{NEWER}version = 0.3.0\n"),
                Refused::Twice {
                    line: 6,
                    key: VERSION,
                },
            ),
            (
                NEWER.replace("sha256 = ", "sha256 := "),
                Refused::Unknown {
                    line: 4,
                    key: String::from("sha256 :"),
                },
            ),
            (
                with("published", ""),
                Refused::Value {
                    line: 2,
                    key: PUBLISHED,
                },
            ),
            (
                NEWER
                    .lines()
                    .filter(|line| !line.starts_with("url"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                Refused::Missing(URL),
            ),
            (NEWER.replace('\n', "\r"), Refused::NotPlainText(1)),
            (
                NEWER.replace("OWNER", "OWN\u{0}ER"),
                Refused::NotPlainText(5),
            ),
            (
                NEWER.replace("OWNER", "\u{41f}WNER"),
                Refused::NotPlainText(5),
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(parse(text.as_bytes()), Err(expected), "{text:?}");
        }
    }

    #[test]
    fn blank_or_bare_equals_line() {
        assert_eq!(refused(&format!("{NEWER}   \n")), Refused::Blank(6));
        assert_eq!(
            refused(&format!("{NEWER}=\n")),
            Refused::Unknown {
                line: 6,
                key: String::new()
            }
        );
    }

    #[test]
    fn versions_compare_as_numbers() {
        let v = |text: &str| text.parse::<Version>().unwrap();
        assert!(v("0.10.0") > v("0.9.0"));
        assert!(v("0.9.10") > v("0.9.9"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert!(v("0.1.1") > v("0.1.0"));
        assert_eq!(v("0.1.0").cmp(&v("0.1.0")), Ordering::Equal);
        assert!(v("0.1.0") < v("0.2.0"));
        assert_eq!(v("999999999.0.0").to_string(), "999999999.0.0");
        assert_eq!(Version::running().to_string(), env!("CARGO_PKG_VERSION"));
        for bad in [
            "", "1", "1.2", "1.2.3.4", "01.2.3", "1..3", "1.2.3 ", "-1.2.3", "1.2.3a",
        ] {
            assert!(bad.parse::<Version>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_signature_from_the_release_key_is_taken() {
        assert_eq!(
            verify(NEWER.as_bytes(), NEWER_SIGNED.as_bytes(), TEST_KEY),
            Ok(())
        );
        assert_eq!(
            verify(SAME.as_bytes(), SAME_SIGNED.as_bytes(), TEST_KEY),
            Ok(())
        );
        let crlf = NEWER_SIGNED.replace('\n', "\r\n");
        assert_eq!(verify(NEWER.as_bytes(), crlf.as_bytes(), TEST_KEY), Ok(()));
    }

    #[test]
    fn a_manifest_changed_by_one_byte_fails_its_signature() {
        let changed = NEWER.replace("0.2.0\npublished", "0.3.0\npublished");
        assert_eq!(
            verify(changed.as_bytes(), NEWER_SIGNED.as_bytes(), TEST_KEY),
            Err(SignatureRefused::Wrong)
        );
        let crlf = NEWER.replace('\n', "\r\n");
        assert_eq!(
            verify(crlf.as_bytes(), NEWER_SIGNED.as_bytes(), TEST_KEY),
            Err(SignatureRefused::Wrong)
        );
        // Another release's signature, from the same key.
        assert_eq!(
            verify(NEWER.as_bytes(), SAME_SIGNED.as_bytes(), TEST_KEY),
            Err(SignatureRefused::Wrong)
        );
        // The trusted comment is signed too.
        let comment = NEWER_SIGNED.replace("1791936000", "1791936001");
        assert_eq!(
            verify(NEWER.as_bytes(), comment.as_bytes(), TEST_KEY),
            Err(SignatureRefused::Wrong)
        );
    }

    #[test]
    fn a_signature_from_any_other_key_is_refused() {
        assert_eq!(
            verify(NEWER.as_bytes(), NEWER_BY_OTHER_KEY.as_bytes(), TEST_KEY),
            Err(SignatureRefused::OtherKey)
        );
        assert_eq!(
            verify(
                NEWER.as_bytes(),
                NEWER_BY_OTHER_KEY_WITH_OUR_ID.as_bytes(),
                TEST_KEY
            ),
            Err(SignatureRefused::Wrong)
        );
    }

    #[test]
    fn minisign_s_own_vectors_hashed_taken_and_legacy_refused() {
        assert_eq!(
            verify(b"test", VECTOR_HASHED.as_bytes(), VECTOR_KEY),
            Ok(())
        );
        assert_eq!(
            verify(b"Test", VECTOR_HASHED.as_bytes(), VECTOR_KEY),
            Err(SignatureRefused::Wrong)
        );
        assert_eq!(
            verify(b"test", VECTOR_LEGACY.as_bytes(), VECTOR_KEY),
            Err(SignatureRefused::Legacy)
        );
        assert_eq!(
            verify(b"test", VECTOR_HASHED.as_bytes(), TEST_KEY),
            Err(SignatureRefused::OtherKey)
        );
    }

    #[test]
    fn no_key_and_no_signature_are_said_plainly() {
        assert_eq!(
            verify(NEWER.as_bytes(), NEWER_SIGNED.as_bytes(), "not made yet"),
            Err(SignatureRefused::NoKey)
        );
        // The real key must decode: a mistyped one would turn every check
        // into "built without the release key".
        assert_eq!(
            verify(
                NEWER.as_bytes(),
                NEWER_SIGNED.as_bytes(),
                crate::update::RELEASE_KEY
            ),
            Err(SignatureRefused::OtherKey)
        );
        for bad in [
            &b""[..],
            b"untrusted comment: x\n",
            b"\xff\xfe",
            b"untrusted comment: x\nnot base64\ntrusted comment: y\nnot base64\n",
        ] {
            assert_eq!(
                verify(NEWER.as_bytes(), bad, TEST_KEY),
                Err(SignatureRefused::Unreadable),
                "{bad:?}"
            );
        }
        let long = format!("{NEWER_SIGNED}{}", " ".repeat(MOST_SIGNATURE_BYTES));
        assert_eq!(
            verify(NEWER.as_bytes(), long.as_bytes(), TEST_KEY),
            Err(SignatureRefused::TooLong)
        );
    }

    fn manifest_text(
        version: Version,
        day: u32,
        digest: [u8; 32],
        path: &str,
        installer: Option<[u8; 32]>,
    ) -> String {
        let zip = zip_name(version);
        let mut text = format!(
            "version = {version}\npublished = 2027-03-{day:02}\nzip = {zip}\nsha256 = {}\nurl = https://example.org/{path}/{zip}\n",
            hex(&digest)
        );
        if let Some(installer) = installer {
            text += &format!(
                "installer = {}\ninstaller_sha256 = {}\n",
                installer_name(version),
                hex(&installer)
            );
        }
        text
    }

    proptest! {
        // Whatever arrives, the reader answers and never panics.
        #[test]
        fn any_bytes_are_answered(bytes in proptest::collection::vec(any::<u8>(), 0..6000)) {
            let _ = parse(&bytes);
        }

        // Lines built from the manifest's own words, in any order and with
        // any values, which reach far deeper than random bytes do.
        #[test]
        fn any_lines_of_known_keys_are_answered(
            lines in proptest::collection::vec(
                (
                    prop::sample::select(vec![
                        "version", "published", "zip", "sha256", "url", "installer", "installer_sha256", "x", "",
                    ]),
                    "[ -~]{0,80}",
                ),
                0..10,
            ),
            crlf in any::<bool>(),
        ) {
            let end = if crlf { "\r\n" } else { "\n" };
            let text: String = lines.iter().map(|(key, value)| format!("{key} = {value}{end}")).collect();
            if let Ok(manifest) = parse(text.as_bytes()) {
                prop_assert_eq!(&manifest.zip, &zip_name(manifest.version));
                prop_assert!(manifest.url.starts_with("https://"));
                if let Some(installer) = &manifest.installer {
                    prop_assert_eq!(&installer.name, &installer_name(manifest.version));
                }
            }
        }

        #[test]
        fn a_good_manifest_reads_back_as_written(
            major in 0u32..1000, minor in 0u32..1000, patch in 0u32..1000,
            day in 1u32..=31,
            digest in any::<[u8; 32]>(),
            path in "[a-z0-9]{1,12}(/[a-zA-Z0-9._~-]{0,8}[a-zA-Z0-9]){0,3}",
            installer in proptest::option::of(any::<[u8; 32]>()),
        ) {
            let version = Version::new(major, minor, patch);
            let text = manifest_text(version, day, digest, &path, installer);
            let manifest = parse(text.as_bytes()).map_err(|why| TestCaseError::fail(format!("{text:?}: {why}")))?;
            prop_assert_eq!(manifest.version, version);
            prop_assert_eq!(manifest.sha256, digest);
            prop_assert_eq!(manifest.zip, zip_name(version));
            prop_assert_eq!(manifest.installer.map(|found| (found.name, found.sha256)), installer.map(|hash| (installer_name(version), hash)));
        }

        // One byte of a good manifest changed to anything else fails the
        // signature, and the reader either refuses it or reads what it now
        // says. The two changes that mean the same are the last line break
        // written as a carriage return or as a space, which the reader trims.
        #[test]
        fn one_changed_byte_never_goes_unnoticed(at in 0usize..NEWER.len(), byte in any::<u8>()) {
            let mut bytes = NEWER.as_bytes().to_vec();
            prop_assume!(bytes[at] != byte);
            bytes[at] = byte;
            prop_assert!(verify(&bytes, NEWER_SIGNED.as_bytes(), TEST_KEY).is_err());
            if let Ok(manifest) = parse(&bytes) {
                let same_meaning = at == NEWER.len() - 1 && (byte == b'\r' || byte == b' ');
                prop_assert_eq!(manifest == good(NEWER), same_meaning);
            }
        }

        #[test]
        fn versions_order_like_their_numbers(a in any::<(u16, u16, u16)>(), b in any::<(u16, u16, u16)>()) {
            let va = Version::new(a.0.into(), a.1.into(), a.2.into());
            let vb = Version::new(b.0.into(), b.1.into(), b.2.into());
            prop_assert_eq!(va.cmp(&vb), a.cmp(&b));
            prop_assert_eq!(va.to_string().parse::<Version>(), Ok(va));
        }
    }
}
