use std::fmt;
use std::time::Duration;

use crate::nvenc::status;
use crate::{Codec, Fit};

#[derive(Debug)]
#[non_exhaustive]
pub enum EncodeError {
    /// No encoder could take the share on this GPU.
    NoEncoderForGpu {
        gpu: String,
        codec: Codec,
        /// Why each encoder tried could not, fastest first, one sentence
        /// each.
        tried: Vec<String>,
        /// Set when the software encoder would take the share at this
        /// smaller size and rate ([`crate::software_fit`]).
        fit: Option<Fit>,
    },
    /// The software encoder was asked for more than 1080p60.
    SoftwareLimit {
        width: u32,
        height: u32,
        fps: u32,
        fit: Fit,
    },
    /// mfplat.dll is not in System32, as on N editions of Windows.
    MediaFoundationMissing,
    MediaFoundationExportMissing {
        name: &'static str,
    },
    /// A Media Foundation call failed. `action` completes "Media Foundation
    /// could not ...".
    MediaFoundation {
        action: &'static str,
        source: windows::core::Error,
    },
    NoHardwareEncoder {
        gpu: String,
        codec: Codec,
    },
    /// Windows' software encoder was asked for HEVC, which it does not do.
    NoSoftwareHevc,
    /// The hardware encoder took a frame and gave nothing back.
    NoOutput {
        index: u64,
        waited: Duration,
    },
    /// A Media Foundation encoder did something its own rules do not allow.
    /// `problem` is the whole sentence.
    EncoderMisbehaved {
        problem: &'static str,
    },
    /// A Media Foundation encoder lacks something Booth needs. `problem` is
    /// the whole sentence.
    EncoderLacks {
        problem: &'static str,
    },
    BadSize {
        width: u32,
        height: u32,
    },
    BadRate {
        fps: u32,
        bitrate: u32,
    },
    /// A Direct3D or DXGI call failed; `action` says what Booth was doing.
    Direct3D {
        action: &'static str,
        source: windows::core::Error,
    },
    NvencMissing,
    NvencLoad {
        source: windows::core::Error,
    },
    NvencExportMissing {
        name: &'static str,
    },
    NvencTooOld {
        major: u32,
        minor: u32,
    },
    /// An NVENC call failed. `action` completes "NVENC could not ...".
    Nvenc {
        action: &'static str,
        status: u32,
        detail: String,
    },
    NvencSizeUnsupported {
        codec: Codec,
        width: u32,
        height: u32,
        min: (u32, u32),
        max: (u32, u32),
    },
    /// The GPU's NVENC does not list the codec, as on GPUs from before
    /// HEVC.
    NvencCodecMissing {
        codec: Codec,
    },
    NvencMissingFeature {
        feature: &'static str,
    },
    WrongFrame {
        problem: String,
    },
    FrameOutOfOrder {
        index: u64,
        previous: u64,
    },
    BitrateChangeUnsupported,
}

// The driver's own text is part of the message rather than a source(), so
// the one line the stats panel shows already says everything known.
impl std::error::Error for EncodeError {}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::NoEncoderForGpu {
                gpu,
                codec,
                tried,
                fit,
            } => {
                // H.264 is what every share falls back to, so without it
                // there is no share at all.
                match codec {
                    Codec::H264 => write!(f, "there is no screen share encoder for the {gpu}")?,
                    Codec::Hevc => {
                        write!(f, "there is no HEVC screen share encoder for the {gpu}")?
                    }
                }
                if !tried.is_empty() {
                    write!(f, ": {}", tried.join("; "))?;
                }
                if let Some(fit) = fit {
                    write!(
                        f,
                        ". Share at {}x{} and {} fps to use the software encoder",
                        fit.width, fit.height, fit.fps
                    )?;
                }
                Ok(())
            }
            EncodeError::SoftwareLimit {
                width,
                height,
                fps,
                fit,
            } => write!(
                f,
                "the software encoder takes up to 1080p at 60 fps and this share is {width}x{height} at {fps}: share at {}x{} and {} fps",
                fit.width, fit.height, fit.fps
            ),
            EncodeError::MediaFoundationMissing => write!(
                f,
                "mfplat.dll was not found in System32: this Windows has no Media Foundation, which N editions leave out. Install the Media Feature Pack under Settings, Apps, Optional features"
            ),
            EncodeError::MediaFoundationExportMissing { name } => write!(
                f,
                "mfplat.dll in System32 has no {name}: Booth needs Windows 10 1803 or later"
            ),
            EncodeError::MediaFoundation { action, source } => write!(
                f,
                "Media Foundation could not {action}: {} ({:#010x})",
                source.message().trim_end_matches(['.', '\r', '\n']),
                source.code().0 as u32
            ),
            EncodeError::NoHardwareEncoder { gpu, codec } => write!(
                f,
                "Windows lists no hardware {codec} encoder for the {gpu}: its graphics driver registers none with Media Foundation"
            ),
            EncodeError::NoSoftwareHevc => write!(
                f,
                "Windows has no software HEVC encoder of its own, so a PC without a hardware one shares in H.264"
            ),
            EncodeError::NoOutput { index, waited } => write!(
                f,
                "the hardware encoder took frame {index} and gave nothing back in {} ms: its driver may have stopped responding",
                waited.as_millis()
            ),
            EncodeError::EncoderMisbehaved { problem } | EncodeError::EncoderLacks { problem } => {
                f.write_str(problem)
            }
            EncodeError::BadSize { width, height } => write!(
                f,
                "cannot encode a {width}x{height} picture: width and height must be even and larger than zero"
            ),
            // offer() has no bitrate to quote, and at 0 fps the bitrate is
            // not what is wrong.
            EncodeError::BadRate { fps: 0, .. } => write!(
                f,
                "cannot encode at 0 frames a second: the frame rate must be larger than zero"
            ),
            EncodeError::BadRate { fps, bitrate } => write!(
                f,
                "cannot encode at {fps} frames a second and {bitrate} bits a second: the bitrate must be at least the frame rate"
            ),
            EncodeError::Direct3D { action, source } => {
                write!(f, "could not {action}: Direct3D said: {source}")
            }
            EncodeError::NvencMissing => write!(
                f,
                "nvEncodeAPI64.dll was not found in System32: this is not an NVIDIA GPU or its driver is not installed"
            ),
            EncodeError::NvencLoad { source } => write!(
                f,
                "could not load nvEncodeAPI64.dll from System32: {source}. Reinstall the NVIDIA graphics driver."
            ),
            EncodeError::NvencExportMissing { name } => write!(
                f,
                "nvEncodeAPI64.dll in System32 has no {name}: the NVIDIA driver is damaged or very old, reinstall it"
            ),
            EncodeError::NvencTooOld { major, minor } => write!(
                f,
                "the NVIDIA driver supports NVENC API {major}.{minor} and Booth needs {}.{}: update the graphics driver",
                crate::nvenc::API_MAJOR,
                crate::nvenc::API_MINOR
            ),
            EncodeError::Nvenc {
                action,
                status: code,
                detail,
            } => {
                write!(
                    f,
                    "NVENC could not {action}: {} ({})",
                    status::meaning(action, *code),
                    status::name(*code)
                )?;
                if !detail.is_empty() {
                    write!(f, ". The driver says: {detail}")?;
                }
                Ok(())
            }
            EncodeError::NvencSizeUnsupported {
                codec,
                width,
                height,
                min,
                max,
            } => write!(
                f,
                "this GPU's NVENC cannot encode {width}x{height} in {codec}: it takes {}x{} up to {}x{}",
                min.0, min.1, max.0, max.1
            ),
            EncodeError::NvencCodecMissing { codec: Codec::Hevc } => write!(
                f,
                "this GPU's NVENC does not encode HEVC, which NVIDIA added with the GTX 900 series: the share goes in H.264"
            ),
            EncodeError::NvencCodecMissing { codec: Codec::H264 } => write!(
                f,
                "this GPU's NVENC does not list H.264 among its codecs: update the graphics driver"
            ),
            EncodeError::NvencMissingFeature { feature } => write!(
                f,
                "this GPU's NVENC has no {feature}, which Booth needs to keep every frame inside one frame interval: update the graphics driver"
            ),
            EncodeError::WrongFrame { problem } => write!(f, "cannot encode this frame: {problem}"),
            EncodeError::FrameOutOfOrder { index, previous } => write!(
                f,
                "cannot encode frame {index} after frame {previous}: frame numbers must go up, and loss recovery depends on it"
            ),
            EncodeError::BitrateChangeUnsupported => write!(
                f,
                "this GPU's NVENC cannot change the bitrate without starting a new stream"
            ),
        }
    }
}
