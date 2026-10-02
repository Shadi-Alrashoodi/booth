use std::fmt;

use super::devices::Direction;
use super::format::FormatError;

// Written the way the rest of Booth writes errors: lower case, what happened
// and then what to do, parts joined with ". " so the panel can show them as
// sentences.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioError {
    // Windows default, and Windows has none.
    NoDevice(Direction),
    // A device chosen in settings that is unplugged or turned off.
    NotConnected {
        direction: Direction,
        name: Option<String>,
    },
    AccessDenied(Direction),
    InUse {
        direction: Direction,
        name: String,
    },
    ServiceStopped(Direction),
    // Unplugged, turned off, or taken over while the stream ran.
    Lost {
        direction: Direction,
        name: String,
    },
    Format {
        direction: Direction,
        name: String,
        error: FormatError,
    },
    // The stream did not come out at 48 kHz, which only a broken driver or
    // a fake device would do.
    Rate {
        direction: Direction,
        name: String,
        rate: u32,
    },
    // Anything else Windows refused. `step` says what Booth was doing, as in
    // "start the microphone".
    Windows {
        step: String,
        code: u32,
        text: String,
    },
    Thread(String),
    // Not from Windows: Booth's last room let go of this device and its
    // stream is still closing, as a Bluetooth headset can for seconds, so
    // the next room waits before it opens it. `default` when the room opens
    // Windows' default, where making another device the default ends the
    // wait; a device chosen in Booth's settings has no such way out.
    StillClosing {
        direction: Direction,
        default: bool,
    },
}

impl AudioError {
    pub fn is_lost(&self) -> bool {
        matches!(self, AudioError::Lost { .. })
    }

    pub fn direction(&self) -> Option<Direction> {
        match self {
            AudioError::NoDevice(direction)
            | AudioError::AccessDenied(direction)
            | AudioError::ServiceStopped(direction)
            | AudioError::NotConnected { direction, .. }
            | AudioError::InUse { direction, .. }
            | AudioError::Lost { direction, .. }
            | AudioError::Format { direction, .. }
            | AudioError::Rate { direction, .. }
            | AudioError::StillClosing { direction, .. } => Some(*direction),
            AudioError::Windows { .. } | AudioError::Thread(_) => None,
        }
    }
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AudioError::NoDevice(Direction::Input) => f.write_str(
                "could not open the microphone: Windows has no microphone turned on. Plug one in, or turn one on in Settings, System, Sound",
            ),
            AudioError::NoDevice(Direction::Output) => f.write_str(
                "could not open the output device: Windows has no speakers or headphones turned on. Plug some in, or turn them on in Settings, System, Sound",
            ),
            AudioError::NotConnected { direction, name } => {
                let what = name.as_deref().unwrap_or("the device chosen in Booth's settings");
                write!(
                    f,
                    "could not open {}: {what} is not connected. Connect it, or choose another device in Booth's settings",
                    direction.noun()
                )
            }
            AudioError::AccessDenied(Direction::Input) => f.write_str(
                "could not open the microphone: Windows says access is denied. Allow microphone access for desktop apps in Settings, Privacy and security, Microphone",
            ),
            AudioError::AccessDenied(Direction::Output) => f.write_str(
                "could not open the output device: Windows says access is denied. Start Booth again, and restart Windows if it keeps happening",
            ),
            AudioError::InUse { direction, name } => write!(
                f,
                "could not open {}: another program has {name} to itself. Close that program, or turn off exclusive control for {name} in the Windows sound settings",
                direction.noun()
            ),
            AudioError::ServiceStopped(direction) => write!(
                f,
                "could not open {}: the Windows Audio service is not running. Start it in Services, or restart Windows",
                direction.noun()
            ),
            AudioError::Lost { name, .. } => write!(
                f,
                "{name} stopped: it was unplugged, turned off, or changed its format. Connect it again, or choose another device in Booth's settings"
            ),
            AudioError::Format {
                direction,
                name,
                error,
            } => write!(
                f,
                "could not open {}: {name} uses {error}, which Booth cannot read. Choose another format for it in the Windows sound settings",
                direction.noun()
            ),
            AudioError::Rate {
                direction,
                name,
                rate,
            } => write!(
                f,
                "could not use {}: {name} delivers {rate} Hz where Booth asked for 48000 Hz. Choose 48000 Hz for it in the Windows sound settings",
                direction.noun()
            ),
            AudioError::Windows { step, code, text } => write!(
                f,
                "could not {step}: {text} ({code:#010x}). Try again, and restart Windows if it keeps happening"
            ),
            AudioError::Thread(err) => write!(
                f,
                "could not start the audio thread: {err}. Close some programs and try again"
            ),
            AudioError::StillClosing { direction, default } => {
                let (closing, way_out) = match direction {
                    Direction::Input => (
                        "the last room's microphone is still closing. It opens here once it lets go",
                        ", or choose another microphone in Settings, System, Sound",
                    ),
                    Direction::Output => (
                        "the last room's speakers are still closing. They open here once they let go",
                        ", or choose other speakers in Settings, System, Sound",
                    ),
                };
                f.write_str(closing)?;
                if *default {
                    f.write_str(way_out)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for AudioError {}
