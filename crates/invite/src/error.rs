use std::fmt;
use std::net::SocketAddr;

use crate::{Candidate, CandidateKind, Version, two_versions};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CodeError {
    Empty,
    NotACode,
    IsReplyCode,
    IsInvite,
    NewerVersion,
    // An invite made by a build of another protocol, which this one cannot join.
    OtherVersion { protocol: u16, version: Version },
    // An invite made by a test build from before there were version numbers.
    Unversioned,
    Damaged,
    // decode never returns this. It is here so a caller that also checks is_expired has one
    // error type to show.
    Expired,
}

impl fmt::Display for CodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            CodeError::OtherVersion { protocol, version } => {
                let (theirs, ours) = two_versions(*version, *protocol);
                return write!(f, "this invite is for Booth {theirs} and you have {ours}");
            }
            CodeError::Empty => "no code was pasted",
            CodeError::NotACode => "this is not a Booth code; Booth codes start with booth1-",
            CodeError::IsReplyCode => {
                "this is a reply code; paste it on the host's screen, not in Join"
            }
            CodeError::IsInvite => "this is an invite, not a reply code; paste it in Join",
            CodeError::NewerVersion => {
                "this code was made by a newer version of Booth; update Booth and paste it again"
            }
            CodeError::Unversioned => {
                "this invite is from a test build of Booth made before the first release"
            }
            CodeError::Damaged => {
                "the code is damaged; copy it again from the message you were sent"
            }
            CodeError::Expired => "this code has expired; ask your friend for a new one",
        };
        f.write_str(text)
    }
}

impl std::error::Error for CodeError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildError {
    Random(getrandom::Error),
    TooManyCandidates(usize),
    BadCandidate {
        candidate: Candidate,
        reason: &'static str,
    },
    BadHostname(&'static str),
    BadAddress {
        addr: SocketAddr,
        reason: &'static str,
    },
    ExpiryOutOfRange(u64),
    VerifiedWithoutMapping,
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::Random(err) => write!(f, "could not get random bytes from Windows: {err}"),
            BuildError::TooManyCandidates(count) => write!(
                f,
                "an invite can carry at most {} addresses, not {count}",
                crate::MAX_CANDIDATES
            ),
            BuildError::BadCandidate { candidate, reason } => write!(
                f,
                "cannot put {} in an invite as {} address: {reason}",
                candidate.addr,
                kind_name(candidate.kind)
            ),
            BuildError::BadHostname(reason) => write!(f, "the host name {reason}"),
            BuildError::BadAddress { addr, reason } => {
                write!(f, "cannot put {addr} in a reply code: {reason}")
            }
            BuildError::ExpiryOutOfRange(at) => write!(
                f,
                "expiry time {at} is past the year 2106, which a code cannot hold; check the system clock"
            ),
            BuildError::VerifiedWithoutMapping => f.write_str(
                "an invite cannot say a port mapping was verified when it says there is no mapping",
            ),
        }
    }
}

impl std::error::Error for BuildError {}

fn kind_name(kind: CandidateKind) -> &'static str {
    match kind {
        CandidateKind::Lan => "a LAN",
        CandidateKind::Vpn => "a VPN",
        CandidateKind::Ipv6 => "an IPv6",
        CandidateKind::Public => "a public",
    }
}
