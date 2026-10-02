#![forbid(unsafe_code)]

mod cookie;
mod error;
mod handshake;
mod mac;
mod noise;
mod packet;
mod replay;
mod session;
mod timestamp;
mod wipe;

pub use cookie::{COOKIE_SECRET_LIFETIME, CookieChecker, read_cookie_reply};
pub use error::SessionError;
pub use handshake::{IncomingInitiation, InitKind, Initiation, invite_psk, read_initiation};
pub use packet::{
    COOKIE_REPLY_LEN, DATA_OVERHEAD, MAX_PLAINTEXT_LEN, PUNCH_LEN, PUNCH_RANDOM_LEN, PacketType,
    cookie_reply_receiver_index, data_receiver_index, is_punch, packet_type, punch_packet,
    response_receiver_index,
};
pub use replay::{REPLAY_WINDOW, ReplayWindow};
pub use session::{
    REJECT_AFTER, REJECT_AFTER_MESSAGES, REKEY_AFTER, REKEY_AFTER_MESSAGES, Received, Sealer,
    Session, Timers,
};
pub use timestamp::{Tai64N, TimestampSource};

pub const NOISE_PATTERN: &str = "Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s";

// Mixed into the handshake hash, so a peer speaking a different protocol version cannot complete a handshake with us.
pub const PROLOGUE: &[u8] = b"booth1";
