//! The encoder interface and the GPU encoders behind it.
//!
//! [`open_codec`] picks the encoder for the GPU a D3D11 device is on. Every
//! encoder takes NV12 textures on that device and gives back one H.264 or
//! HEVC access unit per frame, synchronously: no frame is ever queued inside
//! the encoder. [`offer`] says beforehand which encoders take a codec at a
//! size and rate.

#![deny(unsafe_op_in_unsafe_fn)]

pub use annexb;
mod error;
#[cfg(feature = "fault")]
pub mod fault;
mod gpu;
mod level;
mod mf;
mod nvenc;

use std::fmt;
use std::str::FromStr;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};

pub use error::EncodeError;

/// The codec a share is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Codec {
    /// High profile. Every GPU of the last decade decodes it, and every PC
    /// can encode it, in software if nothing else.
    #[default]
    H264,
    /// Main profile, 8-bit 4:2:0: the same picture in 25 to 40 percent fewer
    /// bits, which pays off only on a busy picture that fills the rate, from
    /// GPUs with a hardware HEVC encoder only.
    /// Windows has no software one without an extension from the Store.
    Hevc,
}

impl Codec {
    pub const ALL: [Codec; 2] = [Codec::H264, Codec::Hevc];

    /// The word the loopback takes on its command line.
    pub fn word(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
        }
    }
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Codec::H264 => "H.264",
            Codec::Hevc => "HEVC",
        })
    }
}

impl FromStr for Codec {
    type Err = String;

    fn from_str(word: &str) -> Result<Codec, String> {
        Codec::ALL
            .into_iter()
            .find(|codec| codec.word().eq_ignore_ascii_case(word))
            .ok_or_else(|| {
                format!("there is no codec called {word:?}: the choices are h264 and hevc")
            })
    }
}

impl From<Codec> for annexb::Codec {
    fn from(codec: Codec) -> annexb::Codec {
        match codec {
            Codec::H264 => annexb::Codec::H264,
            Codec::Hevc => annexb::Codec::Hevc,
        }
    }
}

/// The encoders, fastest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// NVIDIA's encoder through its own driver library.
    Nvenc,
    /// The hardware H.264 or HEVC encoder the GPU driver registers with
    /// Media Foundation. AMD GPUs share through it until Booth has AMF,
    /// which waits for an AMD PC to test it on; every loss costs an IDR.
    /// Intel's opens here as well, but a share on Intel graphics uses the
    /// software encoder unless a test asks for this one.
    MfHardware,
    /// Windows' own software H.264 encoder: every GPU, up to 1080p60, and
    /// the one encoder that copies each frame to system memory.
    MfSoftware,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Nvenc, Kind::MfHardware, Kind::MfSoftware];

    /// The word the loopback and the example take on their command line.
    pub fn word(self) -> &'static str {
        match self {
            Kind::Nvenc => "nvenc",
            Kind::MfHardware => "hardware",
            Kind::MfSoftware => "software",
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Kind::Nvenc => "NVENC",
            Kind::MfHardware => "the Media Foundation hardware encoder",
            Kind::MfSoftware => "the Media Foundation software encoder",
        })
    }
}

impl FromStr for Kind {
    type Err = String;

    fn from_str(word: &str) -> Result<Kind, String> {
        Kind::ALL
            .into_iter()
            .find(|kind| kind.word().eq_ignore_ascii_case(word))
            .ok_or_else(|| {
                format!("there is no encoder called {word:?}: the choices are nvenc, hardware and software")
            })
    }
}

/// A size and frame rate to share at instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fit {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// What the software encoder takes instead of `width` x `height` at `fps`,
/// or None when it takes that as it is. It holds shares to 1080p60's
/// macroblock count and 60 fps, whatever their shape.
///
/// To get that size from capture, ask it for `max_width: fit.width`,
/// `max_height: fit.height` and `max_fps: fit.fps` (capture::Options).
/// Capture scales a picture down until it fits both limits, so a 2560x1440
/// share comes back as 1920x1080, a 3440x1440 one as 2216x928 and a
/// 1440x2560 portrait one as 1080x1920. Both limits, not the height alone:
/// this works the width out from the share's size and capture from the
/// monitor's, and the two can round apart, so the height alone can bring a
/// picture a few pixels wider than the fit. With both, it can come out a
/// few pixels smaller, never larger.
pub fn software_fit(width: u32, height: u32, fps: u32) -> Option<Fit> {
    mf::software_fit(width, height, fps)
}

/// Encoder speed against quality. NVENC's presets by name; the Media
/// Foundation encoders map them onto their own speed setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Preset {
    /// Fastest. The default.
    #[default]
    P1,
    P2,
    P3,
    /// The slowest one Booth offers.
    P4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// Constant bitrate, in bits a second. It works as a ceiling rather than
    /// an average: with a buffer of one frame, NVENC's output on a detailed
    /// test picture runs about 15 percent under it at 15 Mbit/s and 24 under
    /// at 5. The stats panel and the back-off should count the bytes that
    /// come out, not this number.
    pub bitrate: u32,
    pub preset: Preset,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            bitrate: 15_000_000,
            preset: Preset::P1,
        }
    }
}

/// One frame to encode.
pub struct Frame<'a> {
    /// NV12, the size the encoder was opened with, on its device, and fully
    /// written by the time the encoder reads it ([`open_codec`] says how).
    pub texture: &'a ID3D11Texture2D,
    /// Goes up with every frame; loss reports name frames by it.
    pub index: u64,
    /// Encode this frame as an IDR, for example for a viewer who just joined.
    pub force_idr: bool,
}

/// One encoded frame: an H.264 or HEVC access unit in Annex B form, with the
/// parameter sets in front of every IDR (SPS and PPS, and VPS for HEVC).
pub struct AccessUnit {
    pub data: Vec<u8>,
    pub index: u64,
    pub idr: bool,
    /// When the frame went to the encoder.
    pub submitted: Instant,
    /// When its bitstream was ready.
    pub ready: Instant,
}

impl AccessUnit {
    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Submit to bitstream ready: the encode ms in the stats panel.
    pub fn encode_time(&self) -> Duration {
        self.ready - self.submitted
    }
}

/// How the encoder answers a lost frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// From the next frame on nothing refers to the lost frame or anything
    /// after it, so the viewer can keep decoding.
    Invalidated,
    /// The next frame is an IDR; the viewer drops frames until it arrives.
    Idr,
}

pub trait Encoder: Send {
    /// For the stats panel, for example "NVENC H.264 P1" or "NVENC HEVC P1".
    fn name(&self) -> &str;

    /// Which one it is: the stats panel shows the software encoder in warn,
    /// since it caps a share at 1080p60.
    fn kind(&self) -> Kind;

    /// The codec of every access unit it gives back. The sharer writes it
    /// into every frame's header, and the viewer picks its decoder by it,
    /// so every encoder says it and none can inherit a wrong one.
    fn codec(&self) -> Codec;

    /// For the log: why a faster encoder was not used, and for the Media
    /// Foundation encoders which settings this one took and refused, since
    /// each of them takes a different set. Empty when there is nothing to
    /// say.
    fn notes(&self) -> &str {
        ""
    }

    /// Shuts the encoder down now rather than when it is dropped, and says
    /// for the log what did not go: a Media Foundation hardware encoder
    /// whose driver stopped answering can keep its event thread, and with
    /// it the encoder, past the 2 s it is given. None when all of it went.
    /// Nothing is encoded after it.
    fn shut_down(&mut self) -> Option<String> {
        None
    }

    fn encode(&mut self, frame: &Frame<'_>) -> Result<AccessUnit, EncodeError>;

    /// Takes effect on the next frame, never with an IDR. A ceiling, like
    /// [`Settings::bitrate`].
    fn set_bitrate(&mut self, bits_per_second: u32) -> Result<(), EncodeError>;

    /// A viewer lost frame `lost_frame_index`. Several frames lost together
    /// are reported one call each; any order works.
    fn recover(&mut self, lost_frame_index: u64) -> Recovery;

    /// Whether [`Encoder::recover`] would answer a loss of frame
    /// `lost_frame_index` with [`Recovery::Idr`], known before it is called.
    /// Asking commits the encoder to nothing, so the sharer can hold back
    /// an IDR a friend's PC keeps causing and force it with
    /// [`Frame::force_idr`] once it may go, while an invalidation goes at
    /// once. A driver that refuses an invalidation can still turn a false
    /// here into an IDR.
    fn needs_idr(&self, _lost_frame_index: u64) -> bool {
        !self.invalidates()
    }

    /// Whether [`Encoder::recover`] can answer with
    /// [`Recovery::Invalidated`], so the frames after a lost one stay
    /// decodable. The sharer puts this in every frame's header, since the
    /// viewer decides from it whether to hold frames back for an IDR after
    /// a loss.
    fn invalidates(&self) -> bool {
        false
    }
}

/// Opens the fastest encoder for `codec` that works on the GPU `device` is
/// on: NVENC on an NVIDIA GPU, then the hardware encoder the GPU's driver
/// registers with Media Foundation, then, for H.264 only, Windows' software
/// encoder. [`Encoder::notes`] says why a faster one was passed over. A new
/// picture size or codec needs a new encoder; the first frame of every
/// encoder is an IDR. [`offer`] says beforehand whether any of them will.
///
/// When only the software encoder is left and the share is larger than it
/// takes, the error is [`EncodeError::NoEncoderForGpu`] with the size and
/// rate to share at instead (see [`software_fit`] for asking capture for it).
/// Call `open_codec` again at that size rather than opening the software
/// encoder straight away: a GPU encoder that refused the larger share, for
/// example one that stops at level 5.1, may take the smaller one, and it is
/// much faster and reads the frame where it is. A GPU with no hardware HEVC
/// encoder gets [`EncodeError::NoEncoderForGpu`] with no size to fall back
/// to: that share goes in H.264.
///
/// Every [`Encoder::encode`] works through `device`, and a Direct3D 11
/// device's immediate context is not safe to use from two threads at once.
/// Encode on the thread that renders the NV12 texture: that also puts the
/// render ahead of the encode, since one context runs its work in order. To
/// encode on another thread, turn on ID3D11Multithread protection for the
/// device and wait for the render to finish (an event query) first.
///
/// The Media Foundation hardware encoder works on the device from its own
/// threads as well, so opening it turns the device's multithread protection
/// on (capture's device has it on already). The Media Foundation encoders
/// join each thread that calls them to COM's multithreaded apartment, unless
/// the thread is in an apartment already.
pub fn open_codec(
    codec: Codec,
    device: &ID3D11Device,
    width: u32,
    height: u32,
    fps: u32,
    settings: &Settings,
) -> Result<Box<dyn Encoder>, EncodeError> {
    check_request(width, height, fps, settings.bitrate)?;
    let adapter = gpu::adapter_of(device)?;
    let request = Request {
        codec,
        width,
        height,
        fps,
    };
    let mut passed_over: Vec<String> = Vec::new();
    for kind in Kind::ALL {
        if kind == Kind::Nvenc && adapter.vendor != gpu::NVIDIA {
            continue;
        }
        match open_one(kind, &request, device, &adapter, settings, &passed_over) {
            Ok(encoder) => return Ok(encoder),
            Err(EncodeError::SoftwareLimit { fit, .. }) => {
                passed_over.push("the software encoder takes up to 1080p at 60 fps".to_string());
                return Err(EncodeError::NoEncoderForGpu {
                    gpu: adapter.name,
                    codec,
                    tried: passed_over,
                    fit: Some(fit),
                });
            }
            Err(e @ EncodeError::NoSoftwareHevc) => passed_over.push(e.to_string()),
            Err(e) => passed_over.push(format!("{kind} could not open: {e}")),
        }
    }
    Err(EncodeError::NoEncoderForGpu {
        gpu: adapter.name,
        codec,
        tried: passed_over,
        fit: None,
    })
}

/// Opens one encoder and no other, for the loopback and the tests. The rules
/// of [`open_codec`] apply.
pub fn open_kind_codec(
    kind: Kind,
    codec: Codec,
    device: &ID3D11Device,
    width: u32,
    height: u32,
    fps: u32,
    settings: &Settings,
) -> Result<Box<dyn Encoder>, EncodeError> {
    check_request(width, height, fps, settings.bitrate)?;
    let adapter = gpu::adapter_of(device)?;
    if kind == Kind::Nvenc && adapter.vendor != gpu::NVIDIA {
        return Err(EncodeError::NoEncoderForGpu {
            gpu: adapter.name,
            codec,
            tried: vec!["NVENC runs on NVIDIA GPUs only".to_string()],
            fit: None,
        });
    }
    let request = Request {
        codec,
        width,
        height,
        fps,
    };
    open_one(kind, &request, device, &adapter, settings, &[])
}

/// Which encoders on a GPU take a codec at a size and rate, from [`offer`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Offer {
    /// Fastest first, in the order [`open_codec`] tries them.
    pub kinds: Vec<Kind>,
    /// Why each of the others does not, one sentence each, for the log.
    pub refused: Vec<String>,
    /// The name the GPU's driver gives the Media Foundation hardware encoder
    /// [`open_codec`] would start for this codec, for example "NVIDIA HEVC
    /// Encoder MFT", when Windows lists one.
    pub hardware: Option<String>,
}

impl Offer {
    /// Whether [`open_codec`] has an encoder to try.
    pub fn any(&self) -> bool {
        !self.kinds.is_empty()
    }

    pub fn takes(&self, kind: Kind) -> bool {
        self.kinds.contains(&kind)
    }
}

/// Which encoders on the GPU `device` is on offer `codec` at `width` x
/// `height` and `fps`, found without opening an encoder: NVENC's list of
/// codecs and its limits, through a session that encodes nothing, and the
/// hardware encoders Windows lists for the GPU. The limits are the GPU's
/// and the level's; an encoder listed here can still fail to open, for
/// example when a GeForce driver's cap on sessions is reached.
///
/// NVENC's answer goes through `device` as an encoder would, and asking
/// Media Foundation joins the thread to COM as the encoders do
/// ([`open_codec`] says what both ask of the caller's threads). NVENC's
/// session counts against a GeForce driver's cap on open sessions while it
/// lasts. 11 to 42 ms on my RTX 4070 Ti SUPER, so ask once
/// per share, not per frame. Its NVENC session and an encoder opening on
/// another thread take turns, since NVIDIA's Media Foundation encoder can
/// fail to start while an NVENC session opens, so an open may wait for it.
pub fn offer(
    codec: Codec,
    device: &ID3D11Device,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<Offer, EncodeError> {
    // No bitrate to check: the level is chosen for the top of the upload
    // setting, whatever rate a share starts at.
    check_request(width, height, fps, fps)?;
    let adapter = gpu::adapter_of(device)?;
    let request = Request {
        codec,
        width,
        height,
        fps,
    };
    let mut offer = Offer::default();
    let fits = match codec {
        Codec::H264 => level::h264_fits(width, height, fps),
        Codec::Hevc => level::hevc_fits(width, height, fps),
    };
    if !fits {
        offer.refused.push(format!(
            "{width}x{height} at {fps} fps is past every {codec} level, so no encoder takes it"
        ));
        return Ok(offer);
    }
    for kind in Kind::ALL {
        let answer = match kind {
            Kind::Nvenc if adapter.vendor != gpu::NVIDIA => {
                Err("NVENC runs on NVIDIA GPUs only".to_string())
            }
            Kind::Nvenc => nvenc::offers(device, &request),
            Kind::MfHardware => mf::hardware::offers(&adapter, codec).map(|name| {
                offer.hardware = Some(name);
            }),
            Kind::MfSoftware => mf::software::offers(&request),
        };
        match answer {
            Ok(()) => offer.kinds.push(kind),
            Err(reason) => offer.refused.push(reason),
        }
    }
    Ok(offer)
}

/// What an encoder is opened for, the settings aside.
pub(crate) struct Request {
    pub(crate) codec: Codec,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) fps: u32,
}

fn check_request(width: u32, height: u32, fps: u32, bitrate: u32) -> Result<(), EncodeError> {
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(EncodeError::BadSize { width, height });
    }
    if fps == 0 || bitrate < fps {
        return Err(EncodeError::BadRate { fps, bitrate });
    }
    Ok(())
}

// `passed_over` holds why each faster encoder did not open, for the notes.
fn open_one(
    kind: Kind,
    request: &Request,
    device: &ID3D11Device,
    adapter: &gpu::Adapter,
    settings: &Settings,
    passed_over: &[String],
) -> Result<Box<dyn Encoder>, EncodeError> {
    let notes = if passed_over.is_empty() {
        String::new()
    } else {
        format!("{}; using {kind}", passed_over.join("; "))
    };
    Ok(match kind {
        Kind::Nvenc => Box::new(nvenc::Nvenc::open(device, request, settings)?),
        Kind::MfHardware => Box::new(mf::hardware::Hardware::open(
            device, adapter, request, settings, notes,
        )?),
        Kind::MfSoftware => Box::new(mf::software::Software::open(
            device, request, settings, notes,
        )?),
    })
}
