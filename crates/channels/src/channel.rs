use std::fmt;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    Control = 0,
    Chat = 1,
    Voice = 2,
    Video = 3,
    Cursor = 4,
    Input = 5,
    Ping = 6,
}

impl TryFrom<u8> for Channel {
    type Error = FrameError;

    fn try_from(byte: u8) -> Result<Channel, FrameError> {
        Ok(match byte {
            0 => Channel::Control,
            1 => Channel::Chat,
            2 => Channel::Voice,
            3 => Channel::Video,
            4 => Channel::Cursor,
            5 => Channel::Input,
            6 => Channel::Ping,
            other => return Err(FrameError::UnknownChannel(other)),
        })
    }
}

// Appends to out rather than replacing it, so one buffer can be reused and a
// caller can put its own header in front.
pub fn frame(channel: Channel, payload: &[u8], out: &mut Vec<u8>) {
    out.reserve(1 + payload.len());
    out.push(channel as u8);
    out.extend_from_slice(payload);
}

pub fn unframe(plaintext: &[u8]) -> Result<(Channel, &[u8]), FrameError> {
    let (&byte, payload) = plaintext.split_first().ok_or(FrameError::Empty)?;
    Ok((Channel::try_from(byte)?, payload))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    Empty,
    UnknownChannel(u8),
    UnknownKind(u8),
    Length { expected: usize, actual: usize },
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Empty => write!(f, "empty packet"),
            FrameError::UnknownChannel(byte) => write!(f, "unknown channel {byte}"),
            FrameError::UnknownKind(byte) => write!(f, "unknown message kind {byte}"),
            FrameError::Length { expected, actual } => {
                write!(f, "message is {actual} bytes, expected {expected}")
            }
        }
    }
}

impl std::error::Error for FrameError {}
