//! Desktop Duplication of one monitor and the conversion to NV12 on its GPU.

#![deny(unsafe_op_in_unsafe_fn)]

mod cap;
mod convert;
mod device;
mod duplication;
mod error;
mod monitors;
mod pattern;
pub mod reference;
mod timer;

use std::fmt;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;

pub use convert::{Plan, output_size};
pub use device::device_on;
pub use duplication::{Capture, make_process_dpi_aware};
pub use error::{CaptureError, ErrorKind};
pub use monitors::{Adapter, Monitor, MonitorId, Rotation, adapters, monitors};
pub use pattern::{Pattern, pattern_image, read_frame_number};
pub use reference::Nv12Image;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    // A monitor wider or taller than these is scaled down until it fits
    // both, aspect ratio kept. 0 is no limit on that side. H.264 on NVENC
    // takes at most 4096 wide, so a 5120x1440 monitor comes out 4096x1152,
    // while 3840x2160 still comes out 2560x1440.
    pub max_width: u32,
    pub max_height: u32,
    // 0 is no cap: every picture the monitor shows goes out.
    pub max_fps: u32,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            max_width: 4096,
            max_height: 1440,
            max_fps: 120,
        }
    }
}

// One picture ready to encode. The texture is NV12 on the capture device:
// BT.709, limited range, width and height even.
//
// The texture comes from a pool of two. The next call to next() converts
// only into the other one, so an encoder may still be reading this frame
// while it asks for the next; it must be done with it before the call after
// that. A frame held back by the fps cap sits in that other texture, and a
// newer one that replaces it is converted over it, so two are enough.
#[derive(Clone, Debug)]
pub struct Frame {
    pub texture: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
    // Counts the frames that went out from this source, from 0.
    pub number: u64,
    // When DWM put the picture on the monitor: the start of capture to
    // display. For the pattern, when the picture was made.
    pub present: Instant,
    pub acquired: Instant,
    // When the conversion was queued to the GPU.
    pub converted: Instant,
    // Source frames dropped by the fps cap since the previous Frame.
    pub skipped: u32,
    // DXGI's count of presents folded into this picture.
    pub accumulated: u32,
    // Windows blacked out protected content (DRM video) in this picture.
    pub protected: bool,
    pub cursor: Option<CursorUpdate>,
    // GPU time of a recent conversion; it trails by a frame or two, since
    // reading it never waits for the GPU.
    pub gpu_convert: Option<Duration>,
}

#[derive(Clone, Debug)]
pub enum Next {
    Frame(Frame),
    // The pointer moved or changed shape and the picture did not.
    Cursor(CursorUpdate),
    // No new picture for the frame loop's whole wait, 100 ms: the screen is
    // still.
    Idle,
    // Windows took the desktop away. next() keeps trying and returns Idle
    // until it is back, then a Frame.
    Paused(PauseReason),
}

#[derive(Clone, Debug, PartialEq)]
pub struct CursorUpdate {
    // The pointer image's top left corner, in the frame's pixels. It can be
    // negative or past the edge when the pointer is partly off the monitor.
    pub x: i32,
    pub y: i32,
    pub visible: bool,
    // Frame pixels per desktop pixel, for drawing the shape to scale.
    pub scale: f32,
    // Only when the shape changed.
    pub shape: Option<CursorShape>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorKind {
    // 1 bit per pixel, an AND mask followed by an XOR mask, so `height` is
    // twice the pointer's height.
    Monochrome,
    // 32-bit BGRA with alpha.
    Color,
    // 32-bit BGR where the top byte says whether the pixel is XORed with
    // the screen.
    MaskedColor,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorShape {
    pub kind: CursorKind,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PauseReason {
    // A UAC prompt, the lock screen or Ctrl+Alt+Del.
    SecureDesktop,
    // Other programs hold every duplication Windows allows for the monitor.
    Taken,
    // The Windows session is disconnected or locked from Remote Desktop.
    Disconnected,
    // Anything else Windows answered while the desktop was changing; the
    // text says what.
    Changing(String),
}

impl fmt::Display for PauseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PauseReason::SecureDesktop => f.write_str(
                "Windows is showing the secure desktop (a UAC prompt, the lock screen or Ctrl+Alt+Del). Sharing goes on when it closes",
            ),
            PauseReason::Taken => f.write_str(
                "other programs are duplicating this monitor and Windows allows no more. Sharing goes on when one of them stops",
            ),
            PauseReason::Disconnected => f.write_str(
                "this Windows session is disconnected. Sharing goes on when it is back",
            ),
            PauseReason::Changing(what) => write!(f, "{what}. Sharing goes on when Windows lets go"),
        }
    }
}
