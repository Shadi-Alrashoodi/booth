use std::fmt;
use std::io;
use std::path::PathBuf;

use net::BindError;

#[derive(Debug)]
pub enum RoomError {
    Bind(BindError),
    // Checked on this PC's clock only, for an early clear message. The host
    // refuses an expired invite on its own clock either way.
    InviteExpired,
    OwnInvite,
    BadHostKey,
    LocalAddresses(io::Error),
    Start(io::Error),
    Log { path: PathBuf, source: io::Error },
}

impl RoomError {
    pub fn port(&self) -> Option<u16> {
        match self {
            RoomError::Bind(err) => Some(err.port),
            _ => None,
        }
    }

    pub fn is_in_use(&self) -> bool {
        matches!(self, RoomError::Bind(err) if err.is_in_use())
    }
}

impl fmt::Display for RoomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RoomError::Bind(err) => err.fmt(f),
            RoomError::InviteExpired => {
                f.write_str("this invite has expired; ask your friend for a new one")
            }
            RoomError::OwnInvite => f.write_str(
                "this invite was made on this pc; send it to a friend and have them paste it",
            ),
            RoomError::BadHostKey => f.write_str(
                "this invite carries a host key that cannot be used; ask your friend for a new invite",
            ),
            RoomError::LocalAddresses(err) => {
                write!(f, "could not list this pc's network addresses: {err}")
            }
            RoomError::Start(err) => {
                write!(f, "could not start the room's network threads: {err}")
            }
            RoomError::Log { path, source } => {
                write!(f, "could not open the log file {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for RoomError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RoomError::Bind(err) => Some(err),
            RoomError::LocalAddresses(err) | RoomError::Start(err) => Some(err),
            RoomError::Log { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<BindError> for RoomError {
    fn from(err: BindError) -> RoomError {
        RoomError::Bind(err)
    }
}
