// PC sound's codec: stereo Opus in 10 ms frames, CELT only (the restricted
// low-delay application, 2.5 ms of lookahead), at a constant 96 kbit/s, so a
// frame's size says nothing about what plays. Kept apart from voice's codec,
// whose frames, repair data and checks are shaped for speech. Every frame
// from the network goes through FrameInfo::read before libopus sees it.

use std::fmt;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
// 10 ms a channel.
pub const FRAME: usize = 480;
pub const BITRATE: i32 = 96_000;
// What a frame comes to at that rate.
pub const PACKET_BYTES: usize = BITRATE as usize / 8 * FRAME / SAMPLE_RATE as usize;
// The longest frame taken from the network: room for 128 kbit/s later with no
// reader changed.
pub const MAX_PACKET: usize = 160;
// How far the decoded sound trails what went in: the first decoded sample of
// a frame was captured this long before the frame's own first sample.
pub const LOOKAHEAD: usize = 120;

// As voice's: the lowest setting that keeps CELT's pitch pre-filter.
const COMPLEXITY: i32 = 5;

pub struct Encoder {
    opus: opus::Encoder,
}

impl Encoder {
    pub fn new() -> Result<Encoder, StereoError> {
        let mut opus = opus::Encoder::new(
            SAMPLE_RATE,
            opus::Channels::Stereo,
            opus::Application::LowDelay,
        )?;
        opus.set_bitrate(opus::Bitrate::Bits(BITRATE))?;
        opus.set_vbr(false)?;
        opus.set_dtx(false)?;
        opus.set_complexity(COMPLEXITY)?;
        // The listener's speakers past two get the middle of left and right,
        // where intensity stereo's flipped phase would cancel.
        opus.set_phase_inversion_disabled(true)?;
        Ok(Encoder { opus })
    }

    pub fn lookahead(&mut self) -> Result<usize, StereoError> {
        Ok(self.opus.get_lookahead()?.max(0) as usize)
    }

    pub fn phase_inversion_disabled(&mut self) -> Result<bool, StereoError> {
        Ok(self.opus.get_phase_inversion_disabled()?)
    }

    // At the start of a spell: the listener starts its decoder over at a
    // FIRST frame, and the two only decode cleanly when both start clean.
    pub fn reset(&mut self) -> Result<(), StereoError> {
        Ok(self.opus.reset_state()?)
    }

    // One frame, FRAME frames of left and right interleaved, into `out`,
    // which needs room for PACKET_BYTES; exactly that many come back.
    pub fn encode(&mut self, pcm: &[f32], out: &mut [u8]) -> Result<usize, StereoError> {
        if pcm.len() != FRAME * CHANNELS {
            return Err(StereoError::FrameLength {
                expected: FRAME * CHANNELS,
                actual: pcm.len(),
            });
        }
        let actual = out.len();
        let out = out
            .get_mut(..PACKET_BYTES)
            .ok_or(StereoError::BufferTooShort {
                needed: PACKET_BYTES,
                actual,
            })?;
        let len = self.opus.encode_float(pcm, out)?;
        if len < PACKET_BYTES {
            // libopus pads constant-rate frames itself; in case a later one
            // does not, a short frame would tell what plays.
            opus::packet::pad(out, len)?;
        }
        Ok(PACKET_BYTES)
    }
}

pub struct Decoder {
    opus: opus::Decoder,
}

impl Decoder {
    pub fn new() -> Result<Decoder, StereoError> {
        Ok(Decoder {
            opus: opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo)?,
        })
    }

    // One frame read by FrameInfo::read first, into `out`, which needs room
    // for FRAME frames of two samples; the frames written come back.
    pub fn decode(&mut self, packet: &[u8], out: &mut [f32]) -> Result<usize, StereoError> {
        FrameInfo::read(packet)?;
        let out = frame_slice(out)?;
        Ok(self.opus.decode_float(packet, out, false)?)
    }

    // A frame made up for one that never came.
    pub fn conceal(&mut self, out: &mut [f32]) -> Result<usize, StereoError> {
        let out = frame_slice(out)?;
        Ok(self.opus.decode_float(&[], out, false)?)
    }

    pub fn reset(&mut self) -> Result<(), StereoError> {
        Ok(self.opus.reset_state()?)
    }
}

fn frame_slice(out: &mut [f32]) -> Result<&mut [f32], StereoError> {
    let actual = out.len();
    out.get_mut(..FRAME * CHANNELS)
        .ok_or(StereoError::BufferTooShort {
            needed: FRAME * CHANNELS,
            actual,
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameInfo;

impl FrameInfo {
    // Reads the TOC byte (RFC 6716, section 3.1) and takes only what Booth
    // sends: one stereo CELT frame of 10 ms, at any bandwidth, maybe padded
    // by Opus itself (code 3 with a count of one).
    pub fn read(packet: &[u8]) -> Result<FrameInfo, StereoError> {
        let (&toc, rest) = packet.split_first().ok_or(StereoError::Empty)?;
        if packet.len() > MAX_PACKET {
            return Err(StereoError::TooLong(packet.len()));
        }
        let config = toc >> 3;
        if config < 16 {
            return Err(StereoError::NotCelt);
        }
        // CELT's configurations 16 to 31 run 2.5, 5, 10 and 20 ms in turn.
        let samples = [120, 240, 480, 960][usize::from(config % 4)];
        if samples != FRAME {
            return Err(StereoError::NotTenMs(samples));
        }
        if toc & 0x04 == 0 {
            return Err(StereoError::Mono);
        }
        let frames = match toc & 0x03 {
            0 => 1,
            1 | 2 => 2,
            _ => rest.first().map_or(0, |count| usize::from(count & 0x3f)),
        };
        if frames != 1 {
            return Err(StereoError::Frames(frames));
        }
        Ok(FrameInfo)
    }
}

#[derive(Debug)]
pub enum StereoError {
    Opus(opus::Error),
    Empty,
    TooLong(usize),
    NotCelt,
    NotTenMs(usize),
    Mono,
    Frames(usize),
    FrameLength { expected: usize, actual: usize },
    BufferTooShort { needed: usize, actual: usize },
}

impl From<opus::Error> for StereoError {
    fn from(err: opus::Error) -> StereoError {
        StereoError::Opus(err)
    }
}

impl fmt::Display for StereoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StereoError::Opus(err) => write!(
                f,
                "libopus {} failed: {}",
                err.function(),
                err.description()
            ),
            StereoError::Empty => f.write_str("pc sound frame is empty"),
            StereoError::TooLong(len) => write!(
                f,
                "pc sound frame of {len} bytes is longer than the {MAX_PACKET} Booth accepts"
            ),
            StereoError::NotCelt => {
                f.write_str("pc sound frame is not CELT; Booth sends CELT only")
            }
            StereoError::NotTenMs(samples) => write!(
                f,
                "pc sound frame holds a {} ms frame; Booth sends 10 ms",
                *samples as f32 * 1000.0 / SAMPLE_RATE as f32
            ),
            StereoError::Mono => f.write_str("pc sound frame is mono; Booth sends stereo"),
            StereoError::Frames(frames) => write!(
                f,
                "pc sound frame holds {frames} frames; Booth sends one per packet"
            ),
            StereoError::FrameLength { expected, actual } => write!(
                f,
                "the pc sound encoder got {actual} samples; a frame is {expected}"
            ),
            StereoError::BufferTooShort { needed, actual } => {
                write!(f, "buffer holds {actual} where {needed} are needed")
            }
        }
    }
}

impl std::error::Error for StereoError {}
