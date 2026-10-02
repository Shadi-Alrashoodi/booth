//! The one UDP socket, STUN over it, the PC's own addresses, the path word,
//! the firewall rule, and asking the router for a port (PCP, NAT-PMP, UPnP).

// Unsafe is allowed only on the few functions that call Windows directly.
#![deny(unsafe_code)]

mod adapters;
pub mod addrs;
pub mod dns;
pub mod firewall;
mod holder;
pub mod natpmp;
pub mod pace;
pub mod pcp;
mod socket;
pub mod stun;
pub mod upnp;
pub mod watch;

pub use holder::Holder;
pub use pace::{Burst, PaceNumbers, Pacer, Timer};
pub use socket::{BindError, MIN_RECV_BUFFER, Socket};
