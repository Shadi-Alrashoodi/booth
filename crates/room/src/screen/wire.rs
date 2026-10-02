// Video and pointer packets, the payloads of Channel::Video and
// Channel::Cursor, and the cursor shapes that ride the control channel in
// chunks. Every byte here came from a friend's PC, which may be compromised,
// so each packet is read field by field against Booth's own limits, the
// same way talk/wire.rs reads voice. The video packet behind the prefix is
// channels::video's, and its own reader checks it.
//
// Video:   kind u8, slot u8, then the channels::video packet.
// Pointer: kind u8, slot u8, seq u32, x i32, y i32, flags u8, shape u32.
// A sharer sends kind SENT with slot 0 to the host; the host sends RELAYED
// with the sharer's slot to each watcher. Both prefixes are two bytes, so the
// host turns one into the other in place. Numbers are little endian.
//
// A shape goes as control messages of at most SHAPE_CHUNK bytes each:
//   share u32, id u32, total u32, index u16,
//   for index 0 only: kind u8, width u16, height u16, pitch u16,
//     hotspot x i16, hotspot y i16, scale u16 (thousandths),
//   length u16 and the bytes.
// Every chunk carries the total, so each one can be checked on its own: all
// but the last are SHAPE_CHUNK bytes, and the last holds the rest.

use std::fmt;

use channels::reliable::MAX_MESSAGE;
use channels::video::{FRAME_HEADER, Packet, PacketError, read_packet};

use crate::control::{MAX_ROSTER, Reader};

pub(crate) const SENT: u8 = 1;
pub(crate) const RELAYED: u8 = 2;
pub(crate) const PREFIX: usize = 2;

// 1400-byte datagrams when every link is on the LAN, 1200 otherwise, so the
// whole thing fits a 1280-byte tunnel.
pub const LAN_DATAGRAM: usize = 1400;
pub const INTERNET_DATAGRAM: usize = 1200;

// The most a Channel::Video payload may be: a 1400-byte datagram less the
// session's overhead and the channel byte. Anything longer is refused, so
// the host's buffers never grow past one LAN datagram.
pub(crate) const MAX_VIDEO: usize = LAN_DATAGRAM - session::DATA_OVERHEAD - 1;

const POINTER_LEN: usize = PREFIX + 4 + 4 + 4 + 1 + 4;
const VISIBLE: u8 = 1 << 0;
// Frames are at most 4096 pixels wide after scaling (capture's
// Options::max_width, which its Plan::new applies and the room's share
// leaves at its default), and a pointer partly off the monitor is only a
// little past an edge.
const MAX_POSITION: i32 = 1 << 16;

// The largest pointer Windows draws is 256 pixels square; in colour that is
// 256 KB. A monochrome shape is twice as tall, an AND mask over an XOR mask.
pub const MAX_SHAPE_SIDE: u16 = 256;
pub const MAX_SHAPE_BYTES: usize = 256 * 256 * 4;
pub const SHAPE_CHUNK: usize = 1024;
const MAX_PITCH: u16 = MAX_SHAPE_SIDE * 4;
const MAX_SCALE_MILLI: u16 = 8000;
const SHAPE_HEAD: usize = 1 + 2 + 2 + 2 + 2 + 2 + 2;
const SHAPE_MESSAGE_MAX: usize = 1 + 4 + 4 + 4 + 2 + SHAPE_HEAD + 2 + SHAPE_CHUNK;
const _: () = assert!(SHAPE_MESSAGE_MAX <= MAX_MESSAGE);

// Where a pointer is on the shared frame, in the frame's pixels, and which
// shape it has. `seq` goes up by one with each update, so a watcher keeps
// only the newest. Shape 0 is none yet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pointer {
    pub seq: u32,
    pub x: i32,
    pub y: i32,
    pub visible: bool,
    pub shape: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShapeKind {
    // 1 bit per pixel, an AND mask then an XOR mask, so `height` is twice
    // the pointer's.
    Monochrome,
    // 32-bit BGRA with alpha.
    Color,
    // 32-bit BGR whose top byte says whether the pixel is XORed with the
    // screen.
    MaskedColor,
}

// A pointer's picture, as capture::CursorShape has it, with the scale the
// frame was taken at: frame pixels per desktop pixel, in thousandths. Its
// Debug, and a chunk's, show the length of the bytes and not the bytes, so a
// picture taken from the screen never reaches booth.log through a {:?}.
#[derive(Clone, PartialEq, Eq)]
pub struct Shape {
    pub kind: ShapeKind,
    pub width: u16,
    pub height: u16,
    pub pitch: u16,
    pub hotspot_x: i16,
    pub hotspot_y: i16,
    pub scale_milli: u16,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ShapeHead {
    pub kind: ShapeKind,
    pub width: u16,
    pub height: u16,
    pub pitch: u16,
    pub hotspot_x: i16,
    pub hotspot_y: i16,
    pub scale_milli: u16,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ShapeChunk {
    pub share: u32,
    pub id: u32,
    pub total: u32,
    pub index: u16,
    pub head: Option<ShapeHead>,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireError {
    Short,
    Kind(u8),
    Slot(u8),
    TooLong(usize),
    Packet(PacketError),
    Flags(u8),
    Position { x: i32, y: i32 },
    Trailing(usize),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::Short => f.write_str("it ends early"),
            WireError::Kind(kind) => write!(f, "kind {kind} is not one this side takes"),
            WireError::Slot(slot) => write!(f, "slot {slot} is not one this side takes"),
            WireError::TooLong(len) => write!(
                f,
                "{len} bytes; a video payload is at most {MAX_VIDEO}, one {LAN_DATAGRAM}-byte datagram"
            ),
            WireError::Packet(err) => write!(f, "the video packet: {err}"),
            WireError::Flags(flags) => write!(f, "flags {flags:#04x} are not ones Booth sets"),
            WireError::Position { x, y } => write!(
                f,
                "a pointer at {x}, {y}, more than {MAX_POSITION} pixels from the frame"
            ),
            WireError::Trailing(len) => write!(f, "{len} bytes after the end"),
        }
    }
}

// Why a shape is not one Booth sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShapeError {
    Size { width: u16, height: u16 },
    Pitch(u16),
    Bytes { len: usize, expected: usize },
    Hotspot { x: i16, y: i16 },
    Scale(u16),
}

impl fmt::Display for ShapeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShapeError::Size { width, height } => write!(
                f,
                "a pointer of {width}x{height}; Booth takes up to {MAX_SHAPE_SIDE} pixels square"
            ),
            ShapeError::Pitch(pitch) => {
                write!(f, "rows of {pitch} bytes do not fit the pointer's width")
            }
            ShapeError::Bytes { len, expected } => {
                write!(f, "{len} bytes where its size needs {expected}")
            }
            ShapeError::Hotspot { x, y } => write!(f, "a hotspot at {x}, {y}, outside the pointer"),
            ShapeError::Scale(milli) => write!(
                f,
                "a scale of {milli} thousandths; Booth takes 1 to {MAX_SCALE_MILLI}"
            ),
        }
    }
}

impl std::error::Error for ShapeError {}

// What the host takes from the sharer and passes on. `kind` is the kind this
// side takes. A packet whose prefix and length pass still goes through the
// video reader, so a shard that could not be one reaches nobody.
pub(crate) fn read_video(payload: &[u8], kind: u8) -> Result<(u8, Packet<'_>), WireError> {
    if payload.len() > MAX_VIDEO {
        return Err(WireError::TooLong(payload.len()));
    }
    let (slot, rest) = prefix(payload, kind)?;
    let packet = read_packet(rest).map_err(WireError::Packet)?;
    Ok((slot, packet))
}

// The two bytes in front of a video or pointer packet. A sharer's slot is 0:
// the host puts in the one it gave the sharer, whatever the sharer says.
fn prefix(payload: &[u8], kind: u8) -> Result<(u8, &[u8]), WireError> {
    let [got, slot, rest @ ..] = payload else {
        return Err(WireError::Short);
    };
    if *got != kind {
        return Err(WireError::Kind(*got));
    }
    let allowed = match kind {
        SENT => *slot == 0,
        _ => usize::from(*slot) < MAX_ROSTER,
    };
    if !allowed {
        return Err(WireError::Slot(*slot));
    }
    Ok((*slot, rest))
}

pub(crate) fn write_video_prefix(kind: u8, slot: u8, out: &mut Vec<u8>) {
    out.push(kind);
    out.push(slot);
}

pub(crate) fn write_pointer(kind: u8, slot: u8, pointer: &Pointer, out: &mut Vec<u8>) {
    out.push(kind);
    out.push(slot);
    out.extend_from_slice(&pointer.seq.to_le_bytes());
    out.extend_from_slice(&pointer.x.to_le_bytes());
    out.extend_from_slice(&pointer.y.to_le_bytes());
    out.push(if pointer.visible { VISIBLE } else { 0 });
    out.extend_from_slice(&pointer.shape.to_le_bytes());
}

pub(crate) fn read_pointer(payload: &[u8], kind: u8) -> Result<(u8, Pointer), WireError> {
    let (slot, rest) = prefix(payload, kind)?;
    if payload.len() < POINTER_LEN {
        return Err(WireError::Short);
    }
    if payload.len() > POINTER_LEN {
        return Err(WireError::Trailing(payload.len() - POINTER_LEN));
    }
    let word = |at: usize| [rest[at], rest[at + 1], rest[at + 2], rest[at + 3]];
    let seq = u32::from_le_bytes(word(0));
    let x = i32::from_le_bytes(word(4));
    let y = i32::from_le_bytes(word(8));
    let flags = rest[12];
    let shape = u32::from_le_bytes(word(13));
    if flags & !VISIBLE != 0 {
        return Err(WireError::Flags(flags));
    }
    if x.unsigned_abs() > MAX_POSITION as u32 || y.unsigned_abs() > MAX_POSITION as u32 {
        return Err(WireError::Position { x, y });
    }
    Ok((
        slot,
        Pointer {
            seq,
            x,
            y,
            visible: flags & VISIBLE != 0,
            shape,
        },
    ))
}

// The encode time channels::video puts at the front of data shard 0, inside
// the parity (channels/src/video/wire.rs: flags u8, captured u64, encoded
// u64, length u32). Only read, for the strip's jitter: the host passes the
// packet on as it came.
pub(crate) fn encoded_at(packet: &Packet<'_>) -> Option<u64> {
    if packet.index != 0 || packet.shard.len() < FRAME_HEADER {
        return None;
    }
    let bytes = packet.shard.get(9..17)?;
    Some(u64::from_le_bytes(bytes.try_into().ok()?))
}

impl fmt::Debug for Shape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shape")
            .field("kind", &self.kind)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("pitch", &self.pitch)
            .field("hotspot_x", &self.hotspot_x)
            .field("hotspot_y", &self.hotspot_y)
            .field("scale_milli", &self.scale_milli)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

impl fmt::Debug for ShapeChunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShapeChunk")
            .field("share", &self.share)
            .field("id", &self.id)
            .field("total", &self.total)
            .field("index", &self.index)
            .field("head", &self.head)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

impl Shape {
    pub(crate) fn head(&self) -> ShapeHead {
        ShapeHead {
            kind: self.kind,
            width: self.width,
            height: self.height,
            pitch: self.pitch,
            hotspot_x: self.hotspot_x,
            hotspot_y: self.hotspot_y,
            scale_milli: self.scale_milli,
        }
    }

    pub fn check(&self) -> Result<(), ShapeError> {
        let expected = self.head().check()?;
        if self.bytes.len() != expected {
            return Err(ShapeError::Bytes {
                len: self.bytes.len(),
                expected,
            });
        }
        Ok(())
    }
}

impl ShapeHead {
    // The number of bytes a shape of this size has.
    fn check(&self) -> Result<usize, ShapeError> {
        let (rows, row_bytes) = match self.kind {
            // Two masks, each as tall as the pointer.
            ShapeKind::Monochrome if self.height.is_multiple_of(2) => {
                (self.height / 2, self.width.div_ceil(8))
            }
            ShapeKind::Monochrome => (0, 0),
            ShapeKind::Color | ShapeKind::MaskedColor => {
                (self.height, self.width.saturating_mul(4))
            }
        };
        if self.width == 0 || self.width > MAX_SHAPE_SIDE || rows == 0 || rows > MAX_SHAPE_SIDE {
            return Err(ShapeError::Size {
                width: self.width,
                height: self.height,
            });
        }
        if self.pitch < row_bytes || self.pitch > MAX_PITCH {
            return Err(ShapeError::Pitch(self.pitch));
        }
        let inside = |at: i16, side: u16| at >= 0 && (at as u16) < side;
        if !inside(self.hotspot_x, self.width) || !inside(self.hotspot_y, rows) {
            return Err(ShapeError::Hotspot {
                x: self.hotspot_x,
                y: self.hotspot_y,
            });
        }
        if self.scale_milli == 0 || self.scale_milli > MAX_SCALE_MILLI {
            return Err(ShapeError::Scale(self.scale_milli));
        }
        let bytes = usize::from(self.pitch) * usize::from(self.height);
        if bytes > MAX_SHAPE_BYTES {
            return Err(ShapeError::Size {
                width: self.width,
                height: self.height,
            });
        }
        Ok(bytes)
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.push(match self.kind {
            ShapeKind::Monochrome => 0,
            ShapeKind::Color => 1,
            ShapeKind::MaskedColor => 2,
        });
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        out.extend_from_slice(&self.pitch.to_le_bytes());
        out.extend_from_slice(&self.hotspot_x.to_le_bytes());
        out.extend_from_slice(&self.hotspot_y.to_le_bytes());
        out.extend_from_slice(&self.scale_milli.to_le_bytes());
    }

    fn read(r: &mut Reader<'_>) -> Option<ShapeHead> {
        let kind = match r.u8()? {
            0 => ShapeKind::Monochrome,
            1 => ShapeKind::Color,
            2 => ShapeKind::MaskedColor,
            _ => return None,
        };
        Some(ShapeHead {
            kind,
            width: u16::from_le_bytes(r.array()?),
            height: u16::from_le_bytes(r.array()?),
            pitch: u16::from_le_bytes(r.array()?),
            hotspot_x: i16::from_le_bytes(r.array()?),
            hotspot_y: i16::from_le_bytes(r.array()?),
            scale_milli: u16::from_le_bytes(r.array()?),
        })
    }
}

pub(crate) fn chunk_count(total: usize) -> usize {
    total.div_ceil(SHAPE_CHUNK)
}

// How many bytes chunk `index` of a shape of `total` bytes holds.
fn chunk_len(total: usize, index: usize) -> usize {
    total.saturating_sub(index * SHAPE_CHUNK).min(SHAPE_CHUNK)
}

impl ShapeChunk {
    pub(crate) fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.share.to_le_bytes());
        out.extend_from_slice(&self.id.to_le_bytes());
        out.extend_from_slice(&self.total.to_le_bytes());
        out.extend_from_slice(&self.index.to_le_bytes());
        if let Some(head) = &self.head {
            head.write(out);
        }
        let bytes = &self.bytes[..self.bytes.len().min(SHAPE_CHUNK)];
        out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(bytes);
    }

    // None for anything a sharer following these rules would not send.
    pub(crate) fn read(r: &mut Reader<'_>) -> Option<ShapeChunk> {
        let share = u32::from_le_bytes(r.array()?);
        let id = u32::from_le_bytes(r.array()?);
        let total = u32::from_le_bytes(r.array()?);
        let index = u16::from_le_bytes(r.array()?);
        let size = usize::try_from(total).ok()?;
        if share == 0 || id == 0 || size == 0 || size > MAX_SHAPE_BYTES {
            return None;
        }
        if usize::from(index) >= chunk_count(size) {
            return None;
        }
        let head = if index == 0 {
            let head = ShapeHead::read(r)?;
            if head.check().ok()? != size {
                return None;
            }
            Some(head)
        } else {
            None
        };
        let len = usize::from(u16::from_le_bytes(r.array()?));
        if len != chunk_len(size, usize::from(index)) {
            return None;
        }
        Some(ShapeChunk {
            share,
            id,
            total,
            index,
            head,
            bytes: r.bytes(len)?.to_vec(),
        })
    }
}

// A checked shape as the chunks that carry it, in order.
pub(crate) fn chunks(share: u32, id: u32, shape: &Shape) -> impl Iterator<Item = ShapeChunk> + '_ {
    let total = shape.bytes.len() as u32;
    shape
        .bytes
        .chunks(SHAPE_CHUNK)
        .enumerate()
        .map(move |(index, bytes)| ShapeChunk {
            share,
            id,
            total,
            index: index as u16,
            head: (index == 0).then(|| shape.head()),
            bytes: bytes.to_vec(),
        })
}

// A watcher puts a shape back together from its chunks, which the control
// channel brings in order. A chunk out of turn drops the shape under way;
// its first chunk starts one over.
#[derive(Default)]
pub(crate) struct Assembler {
    under_way: Option<(ShapeChunk, u16)>,
}

impl Assembler {
    // The whole shape, with its share and id, once its last chunk is in.
    pub(crate) fn push(&mut self, chunk: ShapeChunk) -> Option<(u32, u32, Shape)> {
        let (mut first, next) = if chunk.index == 0 {
            (chunk, 1)
        } else {
            let (mut first, next) = self.under_way.take()?;
            let same =
                first.share == chunk.share && first.id == chunk.id && first.total == chunk.total;
            if !same || chunk.index != next {
                return None;
            }
            first.bytes.extend_from_slice(&chunk.bytes);
            (first, next + 1)
        };
        if first.bytes.len() < first.total as usize {
            self.under_way = Some((first, next));
            return None;
        }
        let head = first.head.take()?;
        let shape = Shape {
            kind: head.kind,
            width: head.width,
            height: head.height,
            pitch: head.pitch,
            hotspot_x: head.hotspot_x,
            hotspot_y: head.hotspot_y,
            scale_milli: head.scale_milli,
            bytes: first.bytes,
        };
        Some((first.share, first.id, shape))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use channels::video::{FrameFacts, Packetizer};
    use proptest::prelude::*;

    const INTERNET_PAYLOAD: usize = INTERNET_DATAGRAM - 32 - 1 - PREFIX;
    const LAN_PAYLOAD: usize = LAN_DATAGRAM - 32 - 1 - PREFIX;

    fn facts() -> FrameFacts {
        FrameFacts {
            number: 70_000,
            idr: true,
            survives_loss: false,
            hevc: false,
            captured: 1_790_284_323_456_789,
            encoded: 1_790_284_323_459_123,
        }
    }

    // One frame of `len` bytes from the packetizer, each packet with the
    // prefix `kind` and `slot` in front.
    fn frame(len: usize, payload: usize, kind: u8, slot: u8) -> Vec<Vec<u8>> {
        let unit: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
        let mut packetizer = Packetizer::new(payload).unwrap();
        let packets = packetizer.packetize(&facts(), &unit, 20).unwrap();
        packets
            .iter()
            .map(|packet| {
                let mut out = Vec::new();
                write_video_prefix(kind, slot, &mut out);
                out.extend_from_slice(packet);
                out
            })
            .collect()
    }

    #[test]
    fn frame_reads_with_either_prefix() {
        for payload in [INTERNET_PAYLOAD, LAN_PAYLOAD] {
            let sent = frame(5000, payload, SENT, 0);
            assert!(
                sent.iter()
                    .all(|packet| packet.len() + 1 + 32 <= LAN_DATAGRAM)
            );
            for (index, packet) in sent.iter().enumerate() {
                let (slot, read) = read_video(packet, SENT).unwrap();
                assert_eq!(
                    (slot, read.frame, usize::from(read.index)),
                    (0, 70_000, index)
                );
                let expected = (index == 0).then_some(facts().encoded);
                assert_eq!(encoded_at(&read), expected);
            }
            // The host's copy: the same bytes with the two in front turned
            // around.
            let mut relayed = sent[0].clone();
            relayed[0] = RELAYED;
            relayed[1] = 5;
            let (slot, read) = read_video(&relayed, RELAYED).unwrap();
            assert_eq!(slot, 5);
            assert_eq!(read.shard, &sent[0][PREFIX + 10..]);
        }
    }

    #[test]
    fn bad_video_packets_refused() {
        let good = frame(3000, LAN_PAYLOAD, SENT, 0).remove(0);
        let with = |at: usize, byte: u8| {
            let mut bad = good.clone();
            bad[at] = byte;
            bad
        };
        assert_eq!(
            read_video(&with(0, RELAYED), SENT),
            Err(WireError::Kind(RELAYED))
        );
        assert_eq!(read_video(&good, RELAYED), Err(WireError::Kind(SENT)));
        // A sharer does not name a slot; the host gives it one.
        assert_eq!(read_video(&with(1, 3), SENT), Err(WireError::Slot(3)));
        let mut relayed = with(0, RELAYED);
        relayed[1] = MAX_ROSTER as u8;
        assert_eq!(
            read_video(&relayed, RELAYED),
            Err(WireError::Slot(MAX_ROSTER as u8))
        );
        let mut long = good.clone();
        long.resize(MAX_VIDEO + 1, 0);
        assert_eq!(read_video(&long, SENT), Err(WireError::TooLong(long.len())));
        assert!(matches!(
            read_video(&good[..PREFIX + 12], SENT),
            Err(WireError::Packet(_))
        ));
        assert_eq!(read_video(&[SENT], SENT), Err(WireError::Short));
        assert_eq!(read_video(&[], SENT), Err(WireError::Short));
        // A parity count past the data count, from the channels reader.
        let mut parity = good.clone();
        parity[PREFIX + 8..PREFIX + 10].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(matches!(
            read_video(&parity, SENT),
            Err(WireError::Packet(PacketError::ParityCount { .. }))
        ));
    }

    fn pointer_packet(kind: u8, slot: u8, pointer: &Pointer) -> Vec<u8> {
        let mut out = Vec::new();
        write_pointer(kind, slot, pointer, &mut out);
        out
    }

    #[test]
    fn pointer_round_trip_and_errors() {
        let pointer = Pointer {
            seq: u32::MAX,
            x: -12,
            y: 1439,
            visible: true,
            shape: 9,
        };
        let packet = pointer_packet(SENT, 0, &pointer);
        assert_eq!(packet.len(), POINTER_LEN);
        assert_eq!(read_pointer(&packet, SENT), Ok((0, pointer)));
        let relayed = pointer_packet(RELAYED, 2, &pointer);
        assert_eq!(read_pointer(&relayed, RELAYED), Ok((2, pointer)));

        // Offsets: kind 0, slot 1, seq 2, x 6, y 10, flags 14, shape 15.
        let mut flags = packet.clone();
        flags[14] = 0x02;
        assert_eq!(read_pointer(&flags, SENT), Err(WireError::Flags(0x02)));
        let far = Pointer {
            x: MAX_POSITION + 1,
            ..pointer
        };
        assert_eq!(
            read_pointer(&pointer_packet(SENT, 0, &far), SENT),
            Err(WireError::Position {
                x: MAX_POSITION + 1,
                y: 1439
            })
        );
        let mut trailing = packet.clone();
        trailing.push(0);
        assert_eq!(read_pointer(&trailing, SENT), Err(WireError::Trailing(1)));
        for cut in 0..packet.len() {
            assert!(read_pointer(&packet[..cut], SENT).is_err(), "cut at {cut}");
        }
    }

    fn color(width: u16, height: u16) -> Shape {
        let pitch = width * 4;
        Shape {
            kind: ShapeKind::Color,
            width,
            height,
            pitch,
            hotspot_x: 0,
            hotspot_y: 0,
            scale_milli: 667,
            bytes: (0..usize::from(pitch) * usize::from(height))
                .map(|i| (i % 251) as u8)
                .collect(),
        }
    }

    // 32 pixels wide: an AND and an XOR mask of 32 rows, 4 bytes a row.
    fn arrow() -> Shape {
        Shape {
            kind: ShapeKind::Monochrome,
            width: 32,
            height: 64,
            pitch: 4,
            hotspot_x: 1,
            hotspot_y: 1,
            scale_milli: 1000,
            bytes: vec![0x55; 256],
        }
    }

    // Through the chunks and the control channel's encoding, as a watcher
    // gets it.
    fn carried(share: u32, id: u32, shape: &Shape) -> Vec<Option<(u32, u32, Shape)>> {
        let mut assembler = Assembler::default();
        chunks(share, id, shape)
            .map(|chunk| {
                let mut bytes = Vec::new();
                chunk.write(&mut bytes);
                assert!(bytes.len() < MAX_MESSAGE, "with its kind byte, one message");
                let mut r = Reader(&bytes);
                let read = ShapeChunk::read(&mut r).expect("a chunk reads back");
                assert!(r.0.is_empty());
                assembler.push(read)
            })
            .collect()
    }

    #[test]
    fn shapes_round_trip_in_chunks() {
        let largest = color(MAX_SHAPE_SIDE, MAX_SHAPE_SIDE);
        assert_eq!(largest.bytes.len(), MAX_SHAPE_BYTES);
        for shape in [color(1, 1), color(32, 32), color(48, 48), arrow(), largest] {
            shape.check().unwrap();
            let got = carried(4, 77, &shape);
            assert_eq!(got.len(), shape.bytes.len().div_ceil(SHAPE_CHUNK));
            let (last, before) = got.split_last().unwrap();
            assert!(before.iter().all(Option::is_none));
            assert_eq!(last.as_ref(), Some(&(4, 77, shape.clone())));
        }
    }

    #[test]
    fn bad_shapes_refused() {
        let bad = [
            (
                Shape {
                    width: 0,
                    ..color(1, 1)
                },
                "size",
            ),
            (color(257, 1), "size"),
            (color(1, 257), "size"),
            (
                Shape {
                    height: 63,
                    ..arrow()
                },
                "size",
            ),
            (
                Shape {
                    pitch: 3,
                    ..color(1, 1)
                },
                "pitch",
            ),
            (
                Shape {
                    pitch: 2,
                    ..arrow()
                },
                "pitch",
            ),
            (
                Shape {
                    hotspot_x: 1,
                    ..color(1, 1)
                },
                "hotspot",
            ),
            (
                Shape {
                    hotspot_y: 32,
                    ..arrow()
                },
                "hotspot",
            ),
            (
                Shape {
                    hotspot_y: -1,
                    ..arrow()
                },
                "hotspot",
            ),
            (
                Shape {
                    scale_milli: 0,
                    ..arrow()
                },
                "scale",
            ),
            (
                Shape {
                    scale_milli: 8001,
                    ..arrow()
                },
                "scale",
            ),
            (
                Shape {
                    bytes: vec![0; 255],
                    ..arrow()
                },
                "bytes",
            ),
        ];
        for (shape, what) in bad {
            let err = shape.check().expect_err(what);
            let said = match err {
                ShapeError::Size { .. } => "size",
                ShapeError::Pitch(_) => "pitch",
                ShapeError::Hotspot { .. } => "hotspot",
                ShapeError::Scale(_) => "scale",
                ShapeError::Bytes { .. } => "bytes",
            };
            assert_eq!(said, what, "{err}");
        }
    }

    #[test]
    fn chunk_out_of_turn_drops_shape() {
        let shape = color(32, 32);
        let all: Vec<ShapeChunk> = chunks(1, 5, &shape).collect();
        let mut assembler = Assembler::default();
        assert_eq!(assembler.push(all[0].clone()), None);
        assert_eq!(assembler.push(all[2].clone()), None);
        assert_eq!(
            assembler.push(all[3].clone()),
            None,
            "the shape was dropped"
        );
        for chunk in &all[..3] {
            assert_eq!(assembler.push(chunk.clone()), None);
        }
        assert_eq!(assembler.push(all[3].clone()), Some((1, 5, shape.clone())));
        // A chunk of another shape in the middle ends the first.
        let other: Vec<ShapeChunk> = chunks(1, 6, &shape).collect();
        assembler.push(all[0].clone());
        assert_eq!(assembler.push(other[1].clone()), None);
        assert_eq!(assembler.push(all[1].clone()), None);
    }

    #[test]
    fn chunk_reading_rules() {
        let shape = color(32, 32);
        let first = chunks(3, 9, &shape).next().unwrap();
        let mut good = Vec::new();
        first.write(&mut good);
        let read = |bytes: &[u8]| ShapeChunk::read(&mut Reader(bytes));
        assert_eq!(read(&good), Some(first.clone()));
        // Offsets: share 0, id 4, total 8, index 12, head 14, length 27.
        let with = |at: usize, value: &[u8]| {
            let mut bad = good.clone();
            bad[at..at + value.len()].copy_from_slice(value);
            bad
        };
        for bad in [
            with(0, &0u32.to_le_bytes()),
            with(4, &0u32.to_le_bytes()),
            with(8, &0u32.to_le_bytes()),
            with(8, &(MAX_SHAPE_BYTES as u32 + 1).to_le_bytes()),
            // A total that does not match the head's size.
            with(8, &4097u32.to_le_bytes()),
            // A fifth chunk of a shape of four.
            with(12, &4u16.to_le_bytes()),
            with(14, &[3]),
            with(27, &1023u16.to_le_bytes()),
            good[..good.len() - 1].to_vec(),
        ] {
            assert_eq!(read(&bad), None, "{:?}", &bad[..32]);
        }
        // The last chunk holds the rest and nothing more.
        let last = chunks(3, 9, &color(20, 20)).last().unwrap();
        assert_eq!(last.bytes.len(), 1600 - SHAPE_CHUNK);
        let mut bytes = Vec::new();
        last.write(&mut bytes);
        assert_eq!(read(&bytes), Some(last));
    }

    fn any_shape() -> impl Strategy<Value = Shape> {
        (
            prop::sample::select(vec![
                ShapeKind::Monochrome,
                ShapeKind::Color,
                ShapeKind::MaskedColor,
            ]),
            1u16..=64,
            1u16..=64,
            0u16..8,
            1u16..=MAX_SCALE_MILLI,
            any::<u8>(),
        )
            .prop_map(|(kind, width, rows, extra, scale_milli, fill)| {
                let (height, pitch) = match kind {
                    ShapeKind::Monochrome => (rows * 2, width.div_ceil(8) + extra),
                    _ => (rows, width * 4 + extra),
                };
                Shape {
                    kind,
                    width,
                    height,
                    pitch,
                    hotspot_x: (width - 1) as i16,
                    hotspot_y: (rows - 1) as i16,
                    scale_milli,
                    bytes: vec![fill; usize::from(pitch) * usize::from(height)],
                }
            })
    }

    proptest! {
        #[test]
        fn any_pointer_round_trips(
            seq in any::<u32>(),
            x in -MAX_POSITION..=MAX_POSITION,
            y in -MAX_POSITION..=MAX_POSITION,
            visible in any::<bool>(),
            shape in any::<u32>(),
            slot in 0u8..MAX_ROSTER as u8,
        ) {
            let pointer = Pointer { seq, x, y, visible, shape };
            prop_assert_eq!(
                read_pointer(&pointer_packet(SENT, 0, &pointer), SENT),
                Ok((0, pointer))
            );
            prop_assert_eq!(
                read_pointer(&pointer_packet(RELAYED, slot, &pointer), RELAYED),
                Ok((slot, pointer))
            );
        }

        #[test]
        fn any_shape_booth_sends_comes_back_whole(
            shape in any_shape(),
            share in 1u32..,
            id in 1u32..,
        ) {
            prop_assert!(shape.check().is_ok());
            let got = carried(share, id, &shape);
            prop_assert_eq!(got.last().cloned().flatten(), Some((share, id, shape)));
        }

        #[test]
        fn any_bytes_are_read_or_refused(bytes in prop::collection::vec(any::<u8>(), 0..1500)) {
            for kind in [SENT, RELAYED] {
                if let Ok((slot, packet)) = read_video(&bytes, kind) {
                    prop_assert!(bytes.len() <= MAX_VIDEO);
                    prop_assert!(usize::from(slot) < MAX_ROSTER);
                    prop_assert!(packet.shard.len() <= MAX_VIDEO);
                }
                if let Ok((_, pointer)) = read_pointer(&bytes, kind) {
                    let mut again = Vec::new();
                    write_pointer(kind, bytes[1], &pointer, &mut again);
                    prop_assert_eq!(again, bytes.clone());
                }
            }
            if let Some(chunk) = ShapeChunk::read(&mut Reader(&bytes)) {
                prop_assert!(chunk.bytes.len() <= SHAPE_CHUNK);
                prop_assert!(chunk.total as usize <= MAX_SHAPE_BYTES);
            }
        }
    }
}
