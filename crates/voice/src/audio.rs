// Audio in and out: WASAPI in shared mode, so games keep their sound, at the
// smallest period the driver allows. Capture delivers mono f32 at 48 kHz with
// the capture time of each packet as an Instant; render asks for mono f32 at
// 48 kHz and plays it on every channel.
//
// The capture time is an Instant because the room's clock (room::peer::Clock)
// turns an Instant into its wall-anchored microseconds, so a voice packet can
// carry the same kind of timestamp as a ping. On Windows an Instant is the
// performance counter, which is what WASAPI stamps packets with, so the
// conversion is exact.

#![deny(unsafe_code)]

mod devices;
mod error;
pub mod fake;
mod format;
mod level;
mod microphone;
mod stream;
#[allow(unsafe_code)]
mod wasapi;
mod watch;

pub use devices::{Choice, DeviceList, Direction, Endpoint, ListedDevice, Lists, Resolved};
pub use error::AudioError;
pub use format::{Format, FormatError, Plan, RATE, Sample, from_mono, plan, to_mono};
pub use level::Reading;
pub use microphone::{Microphone, hands_free};
pub use stream::{
    Capture, Device, DeviceStream, Event, Packet, REOPEN_AFTER, Render, StreamInfo, StreamThread,
    Wake,
};
pub use wasapi::{EnginePeriods, Probe};
pub use watch::Watch;

// Both device lists as Windows has them now. It starts COM on the calling
// thread, so the panel calls it from a thread of its own, as Watch does.
pub fn lists() -> Result<Lists, AudioError> {
    wasapi::Session::new()?.lists()
}

// The id of Windows' default device on that side now, None when there is
// none. Starts COM on the calling thread, as lists does.
pub fn default_id(direction: Direction) -> Result<Option<String>, AudioError> {
    wasapi::Session::new()?.default_id(direction)
}

// What a stream on this device would get: its engine format, whether Windows
// would resample, and the period. Nothing is opened: the device is asked,
// never started. Starts COM on the calling thread, as lists does.
pub fn probe(direction: Direction, choice: &Choice) -> Result<Probe, AudioError> {
    wasapi::Session::new()?.probe(direction, choice)
}
