// Sample formats as Windows describes them (WAVEFORMATEX and its extensible
// form), and the conversions between them and the mono f32 at 48 kHz that
// the rest of the voice path works in.

use std::fmt;

pub const RATE: u32 = 48_000;

const TAG_PCM: u16 = 1;
const TAG_FLOAT: u16 = 3;
const TAG_EXTENSIBLE: u16 = 0xFFFE;
// WAVEFORMATEX without anything after it.
const BASE_LEN: usize = 18;
// WAVEFORMATEXTENSIBLE: the base, valid bits, channel mask, sub-format GUID.
const EXTENSIBLE_LEN: usize = 40;
const EXTENSIBLE_EXTRA: u16 = 22;
// The sub-format GUIDs for PCM and float differ only in their first field,
// which holds the old format tag; the other 12 bytes are these.
const GUID_TAIL: [u8; 12] = [
    0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];
// Enough for any layout Windows has a speaker bit for.
const MAX_CHANNELS: u16 = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sample {
    F32,
    I16,
    // Three bytes per sample, packed.
    I24,
    // Also 24 valid bits in a 32-bit container, which Windows puts in the
    // high bytes, so it reads the same.
    I32,
}

impl Sample {
    pub fn bytes(self) -> usize {
        match self {
            Sample::I16 => 2,
            Sample::I24 => 3,
            Sample::F32 | Sample::I32 => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub rate: u32,
    pub channels: u16,
    pub sample: Sample,
    // Which speaker each channel is, from the extensible form; 0 when the
    // format did not say.
    pub channel_mask: u32,
}

impl Format {
    // What Booth asks Windows for when the engine does not run at 48 kHz:
    // float at 48 kHz, with the device's own channels, so Windows converts
    // the rate and nothing else.
    pub fn float_48k(channels: u16, channel_mask: u32) -> Format {
        Format {
            rate: RATE,
            channels,
            sample: Sample::F32,
            channel_mask,
        }
    }

    pub fn frame_bytes(&self) -> usize {
        usize::from(self.channels) * self.sample.bytes()
    }

    // A WAVEFORMATEX as the bytes Windows hands over: 18 bytes plus cbSize.
    pub fn parse(bytes: &[u8]) -> Result<Format, FormatError> {
        if bytes.len() < BASE_LEN {
            return Err(FormatError::Short(bytes.len()));
        }
        let tag = u16_at(bytes, 0);
        let channels = u16_at(bytes, 2);
        let rate = u32_at(bytes, 4);
        let block_align = u16_at(bytes, 12);
        let bits = u16_at(bytes, 14);
        let extra = u16_at(bytes, 16);
        if bytes.len() < BASE_LEN + usize::from(extra) {
            return Err(FormatError::Short(bytes.len()));
        }
        let (float, channel_mask) = match tag {
            TAG_PCM => (false, 0),
            TAG_FLOAT => (true, 0),
            TAG_EXTENSIBLE => {
                if extra < EXTENSIBLE_EXTRA || bytes.len() < EXTENSIBLE_LEN {
                    return Err(FormatError::Short(bytes.len()));
                }
                let guid = &bytes[24..40];
                let kind = u32_at(guid, 0);
                if guid[4..] != GUID_TAIL || !(kind == 1 || kind == 3) {
                    return Err(FormatError::SubFormat(guid_text(guid)));
                }
                (kind == 3, u32_at(bytes, 20))
            }
            other => return Err(FormatError::Tag(other)),
        };
        let sample = match (float, bits) {
            (true, 32) => Sample::F32,
            (false, 16) => Sample::I16,
            (false, 24) => Sample::I24,
            (false, 32) => Sample::I32,
            _ => return Err(FormatError::Bits { bits, float }),
        };
        if channels == 0 || channels > MAX_CHANNELS {
            return Err(FormatError::Channels(channels));
        }
        if rate == 0 {
            return Err(FormatError::Rate);
        }
        let format = Format {
            rate,
            channels,
            sample,
            channel_mask,
        };
        if usize::from(block_align) != format.frame_bytes() {
            return Err(FormatError::BlockAlign {
                block_align,
                channels,
                bits,
            });
        }
        Ok(format)
    }

    // The extensible form of this format, ready to hand to Windows.
    pub fn to_bytes(&self) -> [u8; EXTENSIBLE_LEN] {
        let bits = (self.sample.bytes() * 8) as u16;
        let block_align = self.frame_bytes() as u16;
        let mut out = [0u8; EXTENSIBLE_LEN];
        out[0..2].copy_from_slice(&TAG_EXTENSIBLE.to_le_bytes());
        out[2..4].copy_from_slice(&self.channels.to_le_bytes());
        out[4..8].copy_from_slice(&self.rate.to_le_bytes());
        out[8..12].copy_from_slice(&(self.rate * u32::from(block_align)).to_le_bytes());
        out[12..14].copy_from_slice(&block_align.to_le_bytes());
        out[14..16].copy_from_slice(&bits.to_le_bytes());
        out[16..18].copy_from_slice(&EXTENSIBLE_EXTRA.to_le_bytes());
        out[18..20].copy_from_slice(&bits.to_le_bytes());
        out[20..24].copy_from_slice(&self.channel_mask.to_le_bytes());
        let kind: u32 = if self.sample == Sample::F32 { 3 } else { 1 };
        out[24..28].copy_from_slice(&kind.to_le_bytes());
        out[28..40].copy_from_slice(&GUID_TAIL);
        out
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sample = match self.sample {
            Sample::F32 => "32-bit float",
            Sample::I16 => "16-bit",
            Sample::I24 => "24-bit",
            Sample::I32 => "32-bit",
        };
        let khz = f64::from(self.rate) / 1000.0;
        write!(f, "{khz} kHz, {} channels, {sample}", self.channels)
    }
}

// How a stream is opened, from the engine's own format. At 48 kHz the stream
// takes the engine format as it is and can ask for the small period. At any
// other rate Windows converts to 48 kHz float, which only the older call can
// do, at the default period.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    pub format: Format,
    pub resampled: bool,
}

pub fn plan(engine: &Format) -> Plan {
    if engine.rate == RATE {
        Plan {
            format: *engine,
            resampled: false,
        }
    } else {
        Plan {
            format: Format::float_48k(engine.channels, engine.channel_mask),
            resampled: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FormatError {
    Short(usize),
    Tag(u16),
    SubFormat(String),
    Bits {
        bits: u16,
        float: bool,
    },
    Channels(u16),
    Rate,
    BlockAlign {
        block_align: u16,
        channels: u16,
        bits: u16,
    },
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatError::Short(len) => {
                write!(f, "the format description is cut short at {len} bytes")
            }
            FormatError::Tag(tag) => write!(f, "format tag {tag:#06x} is not PCM or float"),
            FormatError::SubFormat(guid) => write!(f, "sub-format {guid} is not PCM or float"),
            FormatError::Bits { bits, float: true } => write!(f, "{bits}-bit float samples"),
            FormatError::Bits { bits, float: false } => write!(f, "{bits}-bit integer samples"),
            FormatError::Channels(n) => write!(f, "{n} channels"),
            FormatError::Rate => write!(f, "a sample rate of 0"),
            FormatError::BlockAlign {
                block_align,
                channels,
                bits,
            } => write!(
                f,
                "a block of {block_align} bytes does not hold {channels} channels of {bits} bits"
            ),
        }
    }
}

impl std::error::Error for FormatError {}

// Appends one sample per frame, the average of the frame's channels.
// Anything after the last whole frame is left out.
pub fn to_mono(data: &[u8], format: &Format, out: &mut Vec<f32>) {
    let frame_bytes = format.frame_bytes();
    let channels = usize::from(format.channels);
    let size = format.sample.bytes();
    let scale = 1.0 / channels as f32;
    out.reserve(data.len() / frame_bytes);
    for frame in data.chunks_exact(frame_bytes) {
        let sum: f32 = frame
            .chunks_exact(size)
            .map(|bytes| read(bytes, format.sample))
            .sum();
        out.push(sum * scale);
    }
}

// Each mono sample to every channel of a frame. `out` holds exactly
// `mono.len()` frames.
pub fn from_mono(mono: &[f32], format: &Format, out: &mut [u8]) {
    let frame_bytes = format.frame_bytes();
    let size = format.sample.bytes();
    debug_assert_eq!(out.len(), mono.len() * frame_bytes);
    for (frame, &value) in out.chunks_exact_mut(frame_bytes).zip(mono) {
        // A NaN from a bad mix would reach the speaker as whatever the
        // driver makes of it; silence is the safe reading.
        let value = if value.is_finite() {
            value.clamp(-1.0, 1.0)
        } else {
            0.0
        };
        let mut bytes = [0u8; 4];
        write(value, format.sample, &mut bytes);
        for slot in frame.chunks_exact_mut(size) {
            slot.copy_from_slice(&bytes[..size]);
        }
    }
}

fn read(bytes: &[u8], sample: Sample) -> f32 {
    match sample {
        Sample::F32 => f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        Sample::I16 => f32::from(i16::from_le_bytes([bytes[0], bytes[1]])) / 32_768.0,
        // Into the top three bytes of an i32, so the sign comes along.
        Sample::I24 => {
            i32::from_le_bytes([0, bytes[0], bytes[1], bytes[2]]) as f32 / 2_147_483_648.0
        }
        Sample::I32 => {
            i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f32 / 2_147_483_648.0
        }
    }
}

fn write(value: f32, sample: Sample, out: &mut [u8; 4]) {
    match sample {
        Sample::F32 => *out = value.to_le_bytes(),
        Sample::I16 => {
            let v = (value * 32_767.0).round() as i16;
            out[..2].copy_from_slice(&v.to_le_bytes());
        }
        Sample::I24 => {
            let v = (f64::from(value) * 8_388_607.0).round() as i32;
            out[..3].copy_from_slice(&v.to_le_bytes()[..3]);
        }
        Sample::I32 => {
            let v = (f64::from(value) * 2_147_483_647.0).round() as i32;
            *out = v.to_le_bytes();
        }
    }
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn guid_text(guid: &[u8]) -> String {
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    format!(
        "{{{:08x}-{:04x}-{:04x}-{}-{}}}",
        u32_at(guid, 0),
        u16_at(guid, 4),
        u16_at(guid, 6),
        hex(&guid[8..10]),
        hex(&guid[10..16])
    )
}
