#![forbid(unsafe_code)]

mod base32;
mod error;
mod invite;
mod reply;
mod text;
mod version;
mod wire;

use std::net::SocketAddr;

pub use error::{BuildError, CodeError};
pub use invite::{Invite, MAX_CANDIDATES, MULTI_USE_SECS, SINGLE_USE_SECS};
pub use reply::{Answers, REPLY_SECS, ReplyCode};
pub use version::{PROTOCOL, RELEASES_PAGE, VERSION, Version, two_versions};
pub use wire::{check_addr, check_hostname};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mapping {
    Easy,
    Hard,
    Unknown,
}

// Lan and Public are IPv4 only, Ipv6 is IPv6 only, and Vpn can be either, because Tailscale and
// WireGuard hand out both. Public is the outside address from STUN or a router mapping. Public
// and Ipv6 must be addresses the internet can reach; Lan and Vpn may be private.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CandidateKind {
    Lan,
    Vpn,
    Ipv6,
    Public,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Candidate {
    pub kind: CandidateKind,
    pub addr: SocketAddr,
}
