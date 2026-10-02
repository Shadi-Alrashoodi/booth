use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionError {
    /// Wrong type byte, wrong length, or reserved bytes that are not zero.
    Malformed,
    BadMac1,
    WrongIndex,
    Handshake(&'static str),
    Encrypt,
    Decrypt,
    /// Also returned for counters that are too old for the replay window.
    Replayed,
    CounterLimit,
    NotConfirmed,
    Expired,
    TooLarge,
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionError::Malformed => f.write_str("malformed packet"),
            SessionError::BadMac1 => f.write_str("bad mac1"),
            SessionError::WrongIndex => f.write_str("wrong receiver index"),
            SessionError::Handshake(reason) => write!(f, "handshake failed: {reason}"),
            SessionError::Encrypt => f.write_str("could not encrypt packet"),
            SessionError::Decrypt => f.write_str("could not decrypt packet"),
            SessionError::Replayed => f.write_str("replayed counter"),
            SessionError::CounterLimit => f.write_str("counter past the limit"),
            SessionError::NotConfirmed => f.write_str("session not confirmed yet"),
            SessionError::Expired => f.write_str("session expired"),
            SessionError::TooLarge => f.write_str("payload too large"),
        }
    }
}

impl std::error::Error for SessionError {}
