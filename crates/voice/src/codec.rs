use std::fmt;

pub const SAMPLE_RATE: u32 = 48_000;
pub const BITRATE: i32 = 32_000;

// The longest frame either mode makes, in samples.
pub const MAX_FRAME: usize = 480;

// Booth's own packets are 20 or 40 bytes. The limit leaves room for a higher
// bitrate later and still bounds what a peer can hand to libopus.
pub const MAX_PACKET: usize = 128;

// 5 is the lowest setting that keeps CELT's pitch pre-filter, which voiced
// speech needs at 20 bytes a frame; above it the gain was under 0.2 dB. In a
// release build on my PC a 5 ms frame encodes in about 35 us and a 10 ms
// repair frame in about 180 us, far inside the 0.5 ms the voice budget gives
// encoding.
const COMPLEXITY: i32 = 5;

// At 32 kbit/s libopus 1.6 leaves the repair data out when told to expect 5
// percent loss or less, and repair data is the point of the 10 ms mode. So
// that mode always tells Opus to expect at least this much.
const REPAIR_MIN_LOSS: u8 = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    // 5 ms frames in Opus's restricted low-delay application (CELT only).
    LowDelay,
    // 10 ms frames in Opus's VoIP application, carrying in-band FEC.
    Repair,
}

impl Mode {
    pub const fn frame_samples(self) -> usize {
        match self {
            Mode::LowDelay => 240,
            Mode::Repair => 480,
        }
    }

    pub const fn frame_ms(self) -> u32 {
        (self.frame_samples() * 1000 / SAMPLE_RATE as usize) as u32
    }

    // At constant rate every packet is exactly this long: 20 bytes for a
    // 5 ms frame, 40 for 10 ms.
    pub const fn packet_bytes(self) -> usize {
        BITRATE as usize / 8 * self.frame_samples() / SAMPLE_RATE as usize
    }

    // How far the decoded audio trails what went in, in samples: 2.5 ms in
    // the low-delay mode, 6.5 ms in the VoIP one. A frame's first decoded
    // sample was spoken this long before the frame's first captured sample,
    // so mouth to ear counts it.
    pub const fn lookahead_samples(self) -> usize {
        match self {
            Mode::LowDelay => 120,
            Mode::Repair => 312,
        }
    }
}

pub struct Encoder {
    opus: opus::Encoder,
    mode: Mode,
    constant_rate: bool,
    loss_percent: u8,
}

impl Encoder {
    pub fn new(mode: Mode, constant_rate: bool) -> Result<Encoder, CodecError> {
        Ok(Encoder {
            opus: configured(mode, constant_rate, 0)?,
            mode,
            constant_rate,
            loss_percent: 0,
        })
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn constant_rate(&self) -> bool {
        self.constant_rate
    }

    // What libopus says the lookahead is, which Mode::lookahead_samples
    // writes down so a receiver knows it without asking.
    pub fn lookahead(&mut self) -> Result<usize, CodecError> {
        Ok(self.opus.get_lookahead()?.max(0) as usize)
    }

    // Opus fixes the application when an encoder is created, so a mode change
    // is a new encoder. Its first packet has no history to predict from, which
    // the far side hears as a small seam, not a gap.
    pub fn set_mode(&mut self, mode: Mode) -> Result<(), CodecError> {
        if mode != self.mode {
            self.opus = configured(mode, self.constant_rate, self.loss_percent)?;
            self.mode = mode;
        }
        Ok(())
    }

    // Called when a transmission starts. The far side's jitter buffer resets
    // its decoder for a transmission that follows an end mark, and the two
    // only decode cleanly when both start from nothing.
    pub fn reset(&mut self) -> Result<(), CodecError> {
        Ok(self.opus.reset_state()?)
    }

    pub fn set_constant_rate(&mut self, on: bool) -> Result<(), CodecError> {
        self.opus.set_vbr(!on)?;
        self.constant_rate = on;
        Ok(())
    }

    // The loss the far side reports. Opus uses it to size its repair data in
    // the 10 ms mode and to lean less on the previous frame in both.
    pub fn set_expected_loss(&mut self, percent: u8) -> Result<(), CodecError> {
        let percent = percent.min(100);
        self.opus
            .set_packet_loss_perc(opus_loss(self.mode, percent))?;
        self.loss_percent = percent;
        Ok(())
    }

    // Encodes one frame of the current mode. `out` needs room for
    // `mode().packet_bytes()`; nothing longer is ever written, which is also
    // what caps the rate when constant rate is off.
    pub fn encode(&mut self, pcm: &[f32], out: &mut [u8]) -> Result<usize, CodecError> {
        let samples = self.mode.frame_samples();
        if pcm.len() != samples {
            return Err(CodecError::FrameLength {
                expected: samples,
                actual: pcm.len(),
            });
        }
        let size = self.mode.packet_bytes();
        let actual = out.len();
        let out = out.get_mut(..size).ok_or(CodecError::BufferTooShort {
            needed: size,
            actual,
        })?;
        let len = self.opus.encode_float(pcm, out)?;
        if self.constant_rate && len < size {
            // libopus pads constant-rate packets itself. This is here in case a
            // later version does not: a short packet would say something about
            // the speech, which is what constant rate is for.
            opus::packet::pad(out, len)?;
            return Ok(size);
        }
        Ok(len)
    }
}

fn configured(
    mode: Mode,
    constant_rate: bool,
    loss_percent: u8,
) -> Result<opus::Encoder, CodecError> {
    let application = match mode {
        Mode::LowDelay => opus::Application::LowDelay,
        Mode::Repair => opus::Application::Voip,
    };
    let mut opus = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, application)?;
    opus.set_bitrate(opus::Bitrate::Bits(BITRATE))?;
    opus.set_vbr(!constant_rate)?;
    // Off is the default. Said anyway: skipping silent frames would show the
    // pauses in the speech on the wire.
    opus.set_dtx(false)?;
    opus.set_complexity(COMPLEXITY)?;
    opus.set_packet_loss_perc(opus_loss(mode, loss_percent))?;
    if mode == Mode::Repair {
        opus.set_inband_fec(true)?;
        // Repair data lives in the SILK layer; the speech hint keeps Opus in
        // SILK or hybrid instead of drifting to CELT, which carries none.
        opus.set_signal(opus::Signal::Voice)?;
    }
    Ok(opus)
}

fn opus_loss(mode: Mode, reported: u8) -> i32 {
    let percent = match mode {
        Mode::LowDelay => reported,
        Mode::Repair => reported.max(REPAIR_MIN_LOSS),
    };
    i32::from(percent)
}

pub struct Decoder {
    opus: opus::Decoder,
}

impl Decoder {
    pub fn new() -> Result<Decoder, CodecError> {
        Ok(Decoder {
            opus: opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono)?,
        })
    }

    pub fn decode(&mut self, packet: &[u8], out: &mut [f32]) -> Result<usize, CodecError> {
        let info = PacketInfo::read(packet)?;
        let out = frame_slice(out, info.samples)?;
        Ok(self.opus.decode_float(packet, out, false)?)
    }

    pub fn reset(&mut self) -> Result<(), CodecError> {
        Ok(self.opus.reset_state()?)
    }

    // Makes up `samples` of audio for a frame that never arrived.
    pub fn conceal(&mut self, samples: usize, out: &mut [f32]) -> Result<usize, CodecError> {
        let out = frame_slice(out, samples)?;
        Ok(self.opus.decode_float(&[], out, false)?)
    }

    // Rebuilds the lost frame just before `next` from the repair data inside
    // `next`. `samples` is the lost frame's length. A packet without repair
    // data gives plain concealment, so this is never worse than `conceal`.
    pub fn recover(
        &mut self,
        next: &[u8],
        samples: usize,
        out: &mut [f32],
    ) -> Result<usize, CodecError> {
        PacketInfo::read(next)?;
        let out = frame_slice(out, samples)?;
        Ok(self.opus.decode_float(next, out, true)?)
    }
}

fn frame_slice(out: &mut [f32], samples: usize) -> Result<&mut [f32], CodecError> {
    if samples != Mode::LowDelay.frame_samples() && samples != Mode::Repair.frame_samples() {
        return Err(CodecError::NotAFrame(samples));
    }
    let actual = out.len();
    out.get_mut(..samples).ok_or(CodecError::BufferTooShort {
        needed: samples,
        actual,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    Silk,
    Hybrid,
    Celt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketInfo {
    pub layer: Layer,
    pub samples: usize,
}

impl PacketInfo {
    // Reads the TOC byte (RFC 6716, section 3.1) and accepts only what Booth
    // sends: one mono frame of 5 or 10 ms.
    pub fn read(packet: &[u8]) -> Result<PacketInfo, CodecError> {
        let (&toc, rest) = packet.split_first().ok_or(CodecError::EmptyPacket)?;
        if packet.len() > MAX_PACKET {
            return Err(CodecError::PacketTooLong(packet.len()));
        }
        let config = usize::from(toc >> 3);
        let (layer, samples) = match config {
            0..=11 => (Layer::Silk, [480, 960, 1920, 2880][config % 4]),
            12..=15 => (Layer::Hybrid, [480, 960][config % 2]),
            _ => (Layer::Celt, [120, 240, 480, 960][config % 4]),
        };
        let stereo = toc & 0x04 != 0;
        // Code 3 is what Opus's own padding produces, with the frame count in
        // the next byte.
        let frames = match toc & 0x03 {
            0 => 1,
            1 | 2 => 2,
            _ => rest.first().map_or(0, |count| usize::from(count & 0x3f)),
        };
        if stereo || frames != 1 || (samples != 240 && samples != 480) {
            return Err(CodecError::UnsupportedPacket {
                frames,
                samples,
                stereo,
            });
        }
        Ok(PacketInfo { layer, samples })
    }

    pub fn mode(&self) -> Mode {
        if self.samples == Mode::LowDelay.frame_samples() {
            Mode::LowDelay
        } else {
            Mode::Repair
        }
    }

    // Only SILK and hybrid frames can carry Opus's repair data for the frame
    // before them.
    pub fn can_carry_repair(&self) -> bool {
        self.layer != Layer::Celt
    }
}

#[derive(Debug)]
pub enum CodecError {
    Opus(opus::Error),
    EmptyPacket,
    PacketTooLong(usize),
    UnsupportedPacket {
        frames: usize,
        samples: usize,
        stereo: bool,
    },
    FrameLength {
        expected: usize,
        actual: usize,
    },
    NotAFrame(usize),
    BufferTooShort {
        needed: usize,
        actual: usize,
    },
}

impl From<opus::Error> for CodecError {
    fn from(err: opus::Error) -> CodecError {
        CodecError::Opus(err)
    }
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::Opus(err) => {
                write!(
                    f,
                    "libopus {} failed: {}",
                    err.function(),
                    err.description()
                )
            }
            CodecError::EmptyPacket => f.write_str("voice packet is empty"),
            CodecError::PacketTooLong(len) => write!(
                f,
                "voice packet of {len} bytes is longer than the {MAX_PACKET} Booth accepts"
            ),
            CodecError::UnsupportedPacket {
                frames,
                samples,
                stereo,
            } => {
                if *stereo {
                    f.write_str("voice packet is stereo; Booth sends mono")
                } else if *frames != 1 {
                    write!(
                        f,
                        "voice packet holds {frames} frames; Booth sends one per packet"
                    )
                } else {
                    write!(
                        f,
                        "voice packet holds a {} ms frame; Booth sends 5 or 10 ms",
                        *samples as f32 * 1000.0 / SAMPLE_RATE as f32
                    )
                }
            }
            CodecError::FrameLength { expected, actual } => write!(
                f,
                "the encoder got {actual} samples; a frame in this mode is {expected}"
            ),
            CodecError::NotAFrame(samples) => write!(
                f,
                "cannot make a frame of {samples} samples; frames are 240 (5 ms) or 480 (10 ms)"
            ),
            CodecError::BufferTooShort { needed, actual } => {
                write!(f, "buffer holds {actual} where {needed} are needed")
            }
        }
    }
}

impl std::error::Error for CodecError {}
