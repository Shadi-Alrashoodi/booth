use blake2::{Blake2s256, Digest};

use crate::wire::SecretBuf;
use crate::{CodeError, PROTOCOL, VERSION, Version, base32};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Invite,
    Reply,
}

pub(crate) const CHECKSUM_LEN: usize = 4;

// The first byte of every body. The "booth1-" prefix names the text format; this byte, which the
// checksum covers, names the body layout. A later version can change one without the other.
// Invites went to layout 2 when the protocol number went in; a build from before that reads a
// layout 2 invite as made by a newer version, which is what it is.
const INVITE_LAYOUT: u8 = 2;
const REPLY_LAYOUT: u8 = 1;
// Only test builds from before version numbers made these.
const UNVERSIONED_INVITE: u8 = 1;

// Every invite layout from 2 on starts with the protocol and the Booth version that made it,
// big-endian, and a later layout must keep them there: then any two builds can name each other's
// version, whatever else changed after these bytes.
pub(crate) const PREAMBLE_LEN: usize = 2 + 3 * 2;

// A later build's code may be bigger than this build's can be. Up to this many bytes it is still
// read as far as its layout and preamble and named as another version rather than as damage. A
// code in this build's own layout is then held to this build's own size.
const LATER_MAX_BODY: usize = 1024;
const _: () =
    assert!(LATER_MAX_BODY >= crate::invite::MAX_BODY && LATER_MAX_BODY >= crate::reply::MAX_BODY);

// Long enough for any prefix a later version could plausibly use plus the largest code read, so
// a newer code is still recognised as newer even when it is bigger than ours can be.
const TEXT_LIMIT: usize = 32 + base32::encoded_len(LATER_MAX_BODY);

impl Kind {
    fn layout(self) -> u8 {
        match self {
            Kind::Invite => INVITE_LAYOUT,
            Kind::Reply => REPLY_LAYOUT,
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Kind::Invite => "booth1-",
            Kind::Reply => "booth1-r-",
        }
    }

    fn domain(self) -> &'static [u8] {
        match self {
            Kind::Invite => b"booth invite",
            Kind::Reply => b"booth reply",
        }
    }

    fn max_body(self) -> usize {
        match self {
            Kind::Invite => crate::invite::MAX_BODY,
            Kind::Reply => crate::reply::MAX_BODY,
        }
    }
}

fn checksum(kind: Kind, body: &[u8]) -> [u8; CHECKSUM_LEN] {
    let hash = Blake2s256::new()
        .chain_update(kind.domain())
        .chain_update(body)
        .finalize();
    let mut sum = [0; CHECKSUM_LEN];
    sum.copy_from_slice(&hash[..CHECKSUM_LEN]);
    sum
}

// What every body of this kind starts with: its layout, and on an invite the preamble with this
// build's protocol and version.
pub(crate) fn start(kind: Kind, body: &mut Vec<u8>) {
    body.push(kind.layout());
    if kind == Kind::Invite {
        body.extend_from_slice(&PROTOCOL.to_be_bytes());
        for part in [VERSION.major, VERSION.minor, VERSION.patch] {
            body.extend_from_slice(&part.to_be_bytes());
        }
    }
}

// Appends the checksum to body, which the caller should have allocated with room for it so the
// invite secret is not left behind in a freed buffer. body begins with what start() wrote.
pub(crate) fn wrap(kind: Kind, body: &mut Vec<u8>) -> String {
    let sum = checksum(kind, body);
    body.extend_from_slice(&sum);
    let mut text = String::with_capacity(kind.prefix().len() + base32::encoded_len(body.len()));
    text.push_str(kind.prefix());
    base32::encode_into(body, &mut text);
    text
}

// Checks everything around the fields and hands parse the fields alone. The copies of the text
// and body made here stay in this function, so they are wiped on every way out of it.
pub(crate) fn open<T>(
    expected: Kind,
    text: &str,
    parse: impl FnOnce(&[u8]) -> Option<T>,
) -> Result<T, CodeError> {
    let (compact, too_long) = compact(text);
    if compact.is_empty() {
        return Err(CodeError::Empty);
    }
    let (kind, data) = split_prefix(&compact)?;
    if kind != expected {
        return Err(match expected {
            Kind::Invite => CodeError::IsReplyCode,
            Kind::Reply => CodeError::IsInvite,
        });
    }
    if too_long || data.len() > base32::encoded_len(LATER_MAX_BODY) {
        return Err(CodeError::Damaged);
    }
    let body = base32::decode(data).ok_or(CodeError::Damaged)?;
    let body_len = body
        .len()
        .checked_sub(CHECKSUM_LEN)
        .ok_or(CodeError::Damaged)?;
    let (content, sum) = body.split_at_checked(body_len).ok_or(CodeError::Damaged)?;
    if checksum(kind, content) != sum {
        return Err(CodeError::Damaged);
    }
    let (&layout, rest) = content.split_first().ok_or(CodeError::Damaged)?;
    let fields = readable(kind, layout, rest)?;
    if body.len() > kind.max_body() {
        return Err(CodeError::Damaged);
    }
    parse(fields).ok_or(CodeError::Damaged)
}

// The checksum held, so a layout this build does not write is not damage but another version's.
// Returns the fields after the layout byte and the preamble when they are this build's to read.
fn readable(kind: Kind, layout: u8, rest: &[u8]) -> Result<&[u8], CodeError> {
    match (kind, layout) {
        (_, 0) => Err(CodeError::Damaged),
        (Kind::Reply, REPLY_LAYOUT) => Ok(rest),
        (Kind::Reply, _) => Err(CodeError::NewerVersion),
        (Kind::Invite, UNVERSIONED_INVITE) => Err(CodeError::Unversioned),
        (Kind::Invite, _) => {
            let (protocol, version, fields) = preamble(rest).ok_or(CodeError::Damaged)?;
            if protocol != PROTOCOL {
                Err(CodeError::OtherVersion { protocol, version })
            } else if layout != INVITE_LAYOUT {
                // Another layout under this protocol: only a later build can have made it.
                Err(CodeError::NewerVersion)
            } else {
                Ok(fields)
            }
        }
    }
}

// No protocol is numbered 0. The version is taken as it comes: it only ever goes into a sentence.
fn preamble(rest: &[u8]) -> Option<(u16, Version, &[u8])> {
    let (head, fields) = rest.split_at_checked(PREAMBLE_LEN)?;
    let part = |at: usize| u16::from_be_bytes([head[at], head[at + 1]]);
    let protocol = part(0);
    if protocol == 0 {
        return None;
    }
    let version = Version {
        major: part(2),
        minor: part(4),
        patch: part(6),
    };
    Some((protocol, version, fields))
}

// Chat apps wrap long lines and people select a little too much. Whitespace and invisible marks
// anywhere, and quotes, angle brackets and trailing punctuation at the ends, are dropped; none of
// them are in the base32 alphabet, and this can never eat part of a code. Every other character
// outside ASCII becomes '?', which nothing accepts, and the rest works on bytes.
fn compact(text: &str) -> (SecretBuf, bool) {
    let trimmed = text
        .trim_start_matches(|c| invisible(c) || wrapper(c))
        .trim_end_matches(|c| invisible(c) || wrapper(c) || matches!(c, '.' | ',' | ';'));
    let mut out = SecretBuf::with_capacity(TEXT_LIMIT);
    for c in trimmed.chars().filter(|&c| !invisible(c)) {
        let byte = match c {
            '\u{2010}' | '\u{2011}' => b'-',
            c => u8::try_from(c).ok().filter(u8::is_ascii).unwrap_or(b'?'),
        };
        if out.len() >= TEXT_LIMIT || out.push(byte).is_none() {
            return (out, true);
        }
    }
    (out, false)
}

// Soft hyphen, zero-width characters, word joiner and byte order mark, plus the direction marks
// chat apps put around pasted text.
fn invisible(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '\u{00ad}'
                | '\u{200b}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2060}'
                | '\u{2066}'..='\u{2069}'
                | '\u{feff}'
        )
}

fn wrapper(c: char) -> bool {
    matches!(
        c,
        '"' | '\'' | '`' | '<' | '>' | '\u{2018}' | '\u{2019}' | '\u{201c}' | '\u{201d}'
    )
}

fn split_prefix(text: &[u8]) -> Result<(Kind, &[u8]), CodeError> {
    let rest = strip_prefix_ignore_case(text, b"booth").ok_or(CodeError::NotACode)?;
    let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    let (version, rest) = rest.split_at_checked(digits).ok_or(CodeError::NotACode)?;
    let rest = rest.strip_prefix(b"-").ok_or(CodeError::NotACode)?;
    match version {
        b"1" => {}
        // No version is numbered 0, and "booth01-" would be a second spelling of booth1.
        [] | [b'0', ..] => return Err(CodeError::NotACode),
        // Anything else is a bigger number, including one too long to parse.
        _ => return Err(CodeError::NewerVersion),
    }
    Ok(match strip_prefix_ignore_case(rest, b"r-") {
        Some(data) => (Kind::Reply, data),
        None => (Kind::Invite, rest),
    })
}

fn strip_prefix_ignore_case<'a>(text: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    let head = text.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        text.get(prefix.len()..)
    } else {
        None
    }
}

#[cfg(test)]
pub(crate) fn seal(kind: Kind, fields: &[u8]) -> String {
    let mut body = Vec::with_capacity(1 + PREAMBLE_LEN + fields.len() + CHECKSUM_LEN);
    start(kind, &mut body);
    body.extend_from_slice(fields);
    wrap(kind, &mut body)
}

#[cfg(test)]
pub(crate) fn fields(kind: Kind, text: &str) -> Vec<u8> {
    open(kind, text, |fields| Some(fields.to_vec())).expect("a valid code")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sealed_with_version(version: u8) -> String {
        let mut body = vec![version, 0, 0];
        wrap(Kind::Reply, &mut body)
    }

    #[test]
    fn body_version_byte() {
        let open_raw = |text: &str| open(Kind::Reply, text, |f| Some(f.to_vec()));
        assert_eq!(open_raw(&sealed_with_version(1)), Ok(vec![0, 0]));
        assert_eq!(open_raw(&sealed_with_version(0)), Err(CodeError::Damaged));
        for later in [2, 9, 255] {
            assert_eq!(
                open_raw(&sealed_with_version(later)),
                Err(CodeError::NewerVersion)
            );
        }
        let mut empty = Vec::new();
        assert_eq!(
            open_raw(&wrap(Kind::Reply, &mut empty)),
            Err(CodeError::Damaged)
        );
    }

    #[test]
    fn compact_never_outgrows_its_buffer() {
        let (text, too_long) = compact(&"\u{e9}".repeat(TEXT_LIMIT * 2));
        assert!(too_long);
        assert_eq!(text.len(), TEXT_LIMIT);
        assert!(text.iter().all(|&b| b == b'?'));
    }

    fn invite_body(layout: u8, protocol: u16, version: [u16; 3], fields: &[u8]) -> String {
        let mut body = vec![layout];
        body.extend_from_slice(&protocol.to_be_bytes());
        for part in version {
            body.extend_from_slice(&part.to_be_bytes());
        }
        body.extend_from_slice(fields);
        wrap(Kind::Invite, &mut body)
    }

    fn open_invite(text: &str) -> Result<Vec<u8>, CodeError> {
        open(Kind::Invite, text, |f| Some(f.to_vec()))
    }

    const THIS: [u16; 3] = [VERSION.major, VERSION.minor, VERSION.patch];

    #[test]
    fn invite_layout_and_protocol() {
        let ours = invite_body(INVITE_LAYOUT, PROTOCOL, THIS, &[5, 6]);
        assert_eq!(open_invite(&ours), Ok(vec![5, 6]));

        // Another Booth version with this protocol reads as this one's.
        let patched = invite_body(INVITE_LAYOUT, PROTOCOL, [VERSION.major, 99, 7], &[5]);
        assert_eq!(open_invite(&patched), Ok(vec![5]));

        // Another protocol is named, whatever follows the preamble, in any later layout.
        for layout in [INVITE_LAYOUT, 3, 255] {
            for fields in [&[][..], &[1, 2, 3][..]] {
                assert_eq!(
                    open_invite(&invite_body(layout, PROTOCOL + 1, [0, 2, 0], fields)),
                    Err(CodeError::OtherVersion {
                        protocol: PROTOCOL + 1,
                        version: Version {
                            major: 0,
                            minor: 2,
                            patch: 0
                        }
                    }),
                    "layout {layout}"
                );
            }
        }
        assert_eq!(
            open_invite(&invite_body(3, PROTOCOL, THIS, &[5, 6])),
            Err(CodeError::NewerVersion)
        );
        assert_eq!(
            open_invite(&invite_body(INVITE_LAYOUT, 0, THIS, &[5, 6])),
            Err(CodeError::Damaged)
        );

        // A layout 1 invite, whatever its body, came from a build before version numbers.
        let mut old = vec![UNVERSIONED_INVITE, 0, 7, 7];
        assert_eq!(
            open_invite(&wrap(Kind::Invite, &mut old)),
            Err(CodeError::Unversioned)
        );
        let mut zero = vec![0, 0, 1];
        assert_eq!(
            open_invite(&wrap(Kind::Invite, &mut zero)),
            Err(CodeError::Damaged)
        );

        // A preamble cut short is damage, in any layout that has one.
        let mut whole = vec![INVITE_LAYOUT];
        whole.extend_from_slice(&PROTOCOL.to_be_bytes());
        whole.extend_from_slice(&[0; 6]);
        for len in 1..whole.len() {
            let mut cut = whole[..len].to_vec();
            assert_eq!(
                open_invite(&wrap(Kind::Invite, &mut cut)),
                Err(CodeError::Damaged),
                "{len} bytes"
            );
        }
    }

    // A later build may put more in an invite than this one ever does.
    #[test]
    fn bigger_later_code_is_named_not_damaged() {
        let later = [0, 2, 0];
        let other_protocol = Err(CodeError::OtherVersion {
            protocol: PROTOCOL + 1,
            version: Version {
                major: 0,
                minor: 2,
                patch: 0,
            },
        });
        let largest = LATER_MAX_BODY - 1 - PREAMBLE_LEN - CHECKSUM_LEN;
        for len in [crate::invite::MAX_BODY, largest] {
            let fields = vec![7; len];
            assert_eq!(
                open_invite(&invite_body(3, PROTOCOL + 1, later, &fields)),
                other_protocol,
                "{len}"
            );
            assert_eq!(
                open_invite(&invite_body(3, PROTOCOL, later, &fields)),
                Err(CodeError::NewerVersion),
                "{len}"
            );
            let mut reply = vec![REPLY_LAYOUT + 1];
            reply.extend_from_slice(&fields);
            assert_eq!(
                open(Kind::Reply, &wrap(Kind::Reply, &mut reply), |f| Some(
                    f.to_vec()
                )),
                Err(CodeError::NewerVersion),
                "{len}"
            );
        }
        let past_any = vec![7; largest + 1];
        assert_eq!(
            open_invite(&invite_body(3, PROTOCOL + 1, later, &past_any)),
            Err(CodeError::Damaged)
        );
        // This build's own layout is held to this build's own size.
        let most = crate::invite::MAX_BODY - 1 - PREAMBLE_LEN - CHECKSUM_LEN;
        let fits = invite_body(INVITE_LAYOUT, PROTOCOL, THIS, &vec![7; most]);
        assert_eq!(open_invite(&fits).map(|f| f.len()), Ok(most));
        let over = invite_body(INVITE_LAYOUT, PROTOCOL, THIS, &vec![7; most + 1]);
        assert_eq!(open_invite(&over), Err(CodeError::Damaged));
    }

    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4096))]

        // Only the layout byte and the protocol decide; the version only names.
        #[test]
        fn any_preamble_gets_its_rule(
            layout in any::<u8>(),
            protocol in prop_oneof![Just(PROTOCOL), Just(0), any::<u16>()],
            version in any::<[u16; 3]>(),
            fields in proptest::collection::vec(any::<u8>(), 0..40),
        ) {
            let got = open_invite(&invite_body(layout, protocol, version, &fields));
            let expected = match layout {
                0 => Err(CodeError::Damaged),
                UNVERSIONED_INVITE => Err(CodeError::Unversioned),
                _ if protocol == 0 => Err(CodeError::Damaged),
                _ if protocol != PROTOCOL => Err(CodeError::OtherVersion {
                    protocol,
                    version: Version { major: version[0], minor: version[1], patch: version[2] },
                }),
                INVITE_LAYOUT => Ok(fields),
                _ => Err(CodeError::NewerVersion),
            };
            prop_assert_eq!(got, expected);
        }
    }
}
