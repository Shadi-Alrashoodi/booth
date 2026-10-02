// Video packets, the payload of Channel::Video after the room's own prefix.
// Every byte came from a friend's PC, which may be compromised, and the
// access unit goes on to FFmpeg, which is C. So a packet is read field by
// field against Booth's own limits, and a frame is refused unless its
// header, its length and its padding are exactly what the packetizer writes.
//
// A packet:
//   frame u32, index u16, data shards u16, parity shards u16, then the shard.
// Data shards are indices 0 to data - 1, parity shards the ones after.
// Every packet of a frame carries the same counts and the same shard length.
// The data count is what the frame needs in shards as big as the sharer's
// datagrams allow; the shard length is then the smallest multiple of
// SHARD_STEP, at least MIN_SHARD, that carries the frame in that many. A
// big frame goes out in as many packets as at the largest length, without
// most of the padding its last shard would have had, and a small one in
// packets of 512 bytes. The lengths are rounded so they tell someone on the
// network path little about what changed on the screen: every frame under
// 492 bytes, a keystroke or a blinking caret, looks the same on the wire,
// and a bigger one shows its size only to within 64 bytes a shard. The
// viewer learns the length from the packets.
//
// The data shards, end to end, are the frame:
//   flags u8, captured u64, encoded u64, length u32, the access unit, zeros
//   to the end of the last shard.
// The parity covers this header too, so a frame rebuilt from parity carries
// its facts with it. Numbers are little endian. The flags say whether the
// frame is an IDR, whether frames after a lost one stay decodable, and
// whether the access unit is HEVC rather than H.264: a share can change
// codec when someone starts or stops watching, and the viewer needs the
// matching decoder from the IDR that starts the new one.

use std::fmt;
use std::ops::Range;

pub const HEADER: usize = 4 + 2 + 2 + 2;
pub const FRAME_HEADER: usize = 1 + 8 + 8 + 4;

// 2048 shards of 1152 bytes (a 1200-byte datagram less 32 bytes of session
// overhead, the channel byte, the room's two-byte prefix and this header,
// rounded down to SHARD_STEP) hold an access unit of 2 359 275 bytes. H.264
// level 4.2 allows one 1080p60 access unit at most 384 x 522 240 / 60 / 2 =
// 1 671 168 bytes (MaxMBPS and MinCR from the standard's table A-1), so the
// worst IDR a software encoder may legally make at 1080p60 fits with room to
// spare.
pub const MAX_DATA: u16 = 2048;

// Every shard is a whole number of these. reed-solomon-simd wants an even
// length, and this is.
pub const SHARD_STEP: usize = 64;

// No datagram Booth sends is over 1400 bytes, so no shard is over the most
// of that in whole steps: 1344, the LAN's largest.
pub const MAX_SHARD: usize = (1400 - HEADER) / SHARD_STEP * SHARD_STEP;

// Every frame under 492 bytes goes in one shard of this length. Data shard
// 0 always holds the whole frame header, so a frame that never completes
// still says whether it was an IDR and what losing it does.
pub const MIN_SHARD: usize = 512;
const _: () = assert!(MIN_SHARD > FRAME_HEADER && MIN_SHARD.is_multiple_of(SHARD_STEP));

const IDR: u8 = 1 << 0;
const SURVIVES_LOSS: u8 = 1 << 1;
const HEVC: u8 = 1 << 2;
const FLAGS: u8 = IDR | SURVIVES_LOSS | HEVC;

// What the sharer knows about a frame besides its bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameFacts {
    // One more for every frame sent, across encoder restarts; wraps.
    pub number: u32,
    pub idr: bool,
    // The encoder invalidates references, so losing a frame that is not an
    // IDR leaves the ones after it decodable; false for an encoder that
    // needs an IDR after any loss. The same on every frame of a stream, IDRs
    // included: a lost IDR needs the next one whatever this says.
    pub survives_loss: bool,
    // The access unit is HEVC; H.264 when false.
    pub hevc: bool,
    // Microseconds on the sharer's ping clock (peer::Clock in the room):
    // when the frame was presented on the sharer's monitor, and when the
    // encoder finished it.
    pub captured: u64,
    pub encoded: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet<'a> {
    pub frame: u32,
    pub index: u16,
    pub data: u16,
    pub parity: u16,
    pub shard: &'a [u8],
}

impl Packet<'_> {
    pub fn write(&self, out: &mut Vec<u8>) {
        write_header(self.frame, self.index, self.data, self.parity, out);
        out.extend_from_slice(self.shard);
    }

    pub fn total(&self) -> usize {
        usize::from(self.data) + usize::from(self.parity)
    }

    pub fn is_parity(&self) -> bool {
        self.index >= self.data
    }
}

pub(crate) fn write_header(frame: u32, index: u16, data: u16, parity: u16, out: &mut Vec<u8>) {
    out.extend_from_slice(&frame.to_le_bytes());
    out.extend_from_slice(&index.to_le_bytes());
    out.extend_from_slice(&data.to_le_bytes());
    out.extend_from_slice(&parity.to_le_bytes());
}

pub fn read_packet(bytes: &[u8]) -> Result<Packet<'_>, PacketError> {
    let short = PacketError::Short(bytes.len());
    let (header, shard) = bytes.split_first_chunk::<HEADER>().ok_or(short)?;
    let mut r = Fields(header);
    let frame = u32::from_le_bytes(r.take().ok_or(short)?);
    let index = u16::from_le_bytes(r.take().ok_or(short)?);
    let data = u16::from_le_bytes(r.take().ok_or(short)?);
    let parity = u16::from_le_bytes(r.take().ok_or(short)?);
    if !(MIN_SHARD..=MAX_SHARD).contains(&shard.len()) || !shard.len().is_multiple_of(SHARD_STEP) {
        return Err(PacketError::Shard(shard.len()));
    }
    if data == 0 || data > MAX_DATA {
        return Err(PacketError::DataCount(data));
    }
    if parity == 0 || parity > data {
        return Err(PacketError::ParityCount { parity, data });
    }
    if u32::from(index) >= u32::from(data) + u32::from(parity) {
        return Err(PacketError::Index {
            index,
            total: u32::from(data) + u32::from(parity),
        });
    }
    Ok(Packet {
        frame,
        index,
        data,
        parity,
        shard,
    })
}

// The smallest number of shards that holds a frame header and `len` bytes.
pub(crate) fn data_shards(len: usize, shard: usize) -> usize {
    (FRAME_HEADER + len).div_ceil(shard)
}

// The shortest shard that carries a frame header and `len` bytes in `data`
// shards, in whole steps and never under MIN_SHARD. With the count the
// packetizer picks, this is at most its largest shard, which is a whole
// number of steps too, so fewer shards of this length could not carry the
// frame either: data_shards gives the same count back, as read_frame
// checks.
pub(crate) fn shard_for(len: usize, data: usize) -> usize {
    (FRAME_HEADER + len)
        .div_ceil(data.max(1))
        .next_multiple_of(SHARD_STEP)
        .max(MIN_SHARD)
}

pub(crate) fn write_frame_header(facts: &FrameFacts, len: u32, out: &mut [u8]) {
    let mut flags = 0;
    if facts.idr {
        flags |= IDR;
    }
    if facts.survives_loss {
        flags |= SURVIVES_LOSS;
    }
    if facts.hevc {
        flags |= HEVC;
    }
    out[0] = flags;
    out[1..9].copy_from_slice(&facts.captured.to_le_bytes());
    out[9..17].copy_from_slice(&facts.encoded.to_le_bytes());
    out[17..21].copy_from_slice(&len.to_le_bytes());
}

// IDR and survives-loss alone, from data shard 0 of a frame that may never
// complete. The codec does not matter for a frame that is lost.
pub(crate) fn read_flags(shard0: &[u8]) -> Option<(bool, bool)> {
    let flags = *shard0.first()?;
    (flags & !FLAGS == 0).then_some((flags & IDR != 0, flags & SURVIVES_LOSS != 0))
}

// `frame` is the data shards end to end. Returns the facts, less the frame
// number, and where the access unit is.
pub(crate) fn read_frame(
    frame: &[u8],
    shard: usize,
) -> Result<(FrameFacts, Range<usize>), PacketError> {
    let short = PacketError::Short(frame.len());
    let mut r = Fields(frame);
    let [flags] = r.take().ok_or(short)?;
    let captured = u64::from_le_bytes(r.take().ok_or(short)?);
    let encoded = u64::from_le_bytes(r.take().ok_or(short)?);
    let len = u32::from_le_bytes(r.take().ok_or(short)?);
    if flags & !FLAGS != 0 {
        return Err(PacketError::Flags(flags));
    }
    let data = frame.len() / shard;
    // Longer shards than the packetizer cuts would carry the frame too,
    // with more padding, and so would more shards than their length needs.
    // They are refused like anything else Booth does not send, so a frame
    // has one form for its data count.
    let fits = usize::try_from(len)
        .ok()
        .filter(|&len| len > 0 && data_shards(len, shard) == data && shard_for(len, data) == shard);
    let Some(len) = fits else {
        return Err(PacketError::Length { len, data, shard });
    };
    let end = FRAME_HEADER + len;
    if frame[end..].iter().any(|&byte| byte != 0) {
        return Err(PacketError::Padding);
    }
    let facts = FrameFacts {
        number: 0,
        idr: flags & IDR != 0,
        survives_loss: flags & SURVIVES_LOSS != 0,
        hevc: flags & HEVC != 0,
        captured,
        encoded,
    };
    Ok((facts, FRAME_HEADER..end))
}

// Bounds-checked reads that return None past the end, so nothing here
// indexes or unwraps hostile bytes.
struct Fields<'a>(&'a [u8]);

impl Fields<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, tail) = self.0.split_first_chunk::<N>()?;
        self.0 = tail;
        Some(*head)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketError {
    Short(usize),
    Shard(usize),
    DataCount(u16),
    ParityCount {
        parity: u16,
        data: u16,
    },
    Index {
        index: u16,
        total: u32,
    },
    // A packet whose counts or shard length are not the ones the frame's
    // first packet had.
    Mismatch {
        data: u16,
        parity: u16,
        shard: usize,
        first: (u16, u16, usize),
    },
    Flags(u8),
    Length {
        len: u32,
        data: usize,
        shard: usize,
    },
    Padding,
}

impl fmt::Display for PacketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            PacketError::Short(len) => write!(f, "it ends early, at {len} bytes"),
            PacketError::Shard(len) => write!(
                f,
                "a shard of {len} bytes; Booth sends a multiple of {SHARD_STEP} from {MIN_SHARD} \
                 to {MAX_SHARD}"
            ),
            PacketError::DataCount(data) => {
                write!(f, "{data} data shards; Booth sends 1 to {MAX_DATA}")
            }
            PacketError::ParityCount { parity, data } => write!(
                f,
                "{parity} parity shards for {data} data shards; Booth sends 1 to as many as the data"
            ),
            PacketError::Index { index, total } => {
                write!(f, "shard {index} of a frame of {total}")
            }
            PacketError::Mismatch {
                data,
                parity,
                shard,
                first: (first_data, first_parity, first_shard),
            } => write!(
                f,
                "{data} data and {parity} parity shards of {shard} bytes, where the frame's \
                 first packet said {first_data} and {first_parity} of {first_shard} bytes"
            ),
            PacketError::Flags(flags) => {
                write!(f, "frame flags {flags:#04x} are not ones Booth sets")
            }
            PacketError::Length { len, data, shard } => write!(
                f,
                "a frame of {len} bytes in {data} shards of {shard} bytes, which is not how \
                 Booth cuts one"
            ),
            PacketError::Padding => f.write_str("the padding after the frame is not zero bytes"),
        }
    }
}

impl std::error::Error for PacketError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> FrameFacts {
        FrameFacts {
            number: 0,
            idr: true,
            survives_loss: false,
            hevc: false,
            captured: 0x0102_0304_0506_0708,
            encoded: 0x1112_1314_1516_1718,
        }
    }

    // A frame of `len` bytes cut as the packetizer cuts it for shards of at
    // most `full` bytes, and the shard length it got.
    fn frame_of(len: usize, full: usize) -> (Vec<u8>, usize) {
        let data = data_shards(len, full);
        let shard = shard_for(len, data);
        let mut frame = vec![0; data * shard];
        write_frame_header(&facts(), len as u32, &mut frame);
        for (i, byte) in frame[FRAME_HEADER..FRAME_HEADER + len]
            .iter_mut()
            .enumerate()
        {
            *byte = (i * 7 + 1) as u8;
        }
        (frame, shard)
    }

    #[test]
    fn frame_header_layout() {
        let (frame, shard) = frame_of(3, MIN_SHARD);
        assert_eq!(shard, MIN_SHARD);
        assert_eq!(frame[0], IDR);
        assert_eq!(frame[1..9], [8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(
            frame[9..17],
            [0x18, 0x17, 0x16, 0x15, 0x14, 0x13, 0x12, 0x11]
        );
        assert_eq!(frame[17..21], [3, 0, 0, 0]);
        assert!(frame[24..].iter().all(|&byte| byte == 0));
        assert_eq!(read_frame(&frame, MIN_SHARD), Ok((facts(), 21..24)));
        assert_eq!(read_flags(&frame), Some((true, false)));
    }

    // Every combination of the three flags comes back as it went, the codec
    // with it, and a frame lost with only its first shard still says what
    // losing it does.
    #[test]
    fn flags_round_trip() {
        for bits in 0..8u8 {
            let facts = FrameFacts {
                idr: bits & 1 != 0,
                survives_loss: bits & 2 != 0,
                hevc: bits & 4 != 0,
                ..facts()
            };
            let mut frame = vec![0; MIN_SHARD];
            write_frame_header(&facts, 3, &mut frame);
            assert_eq!(frame[0], bits);
            assert_eq!(read_frame(&frame, MIN_SHARD), Ok((facts, 21..24)));
            assert_eq!(
                read_flags(&frame),
                Some((facts.idr, facts.survives_loss)),
                "{bits:#04x}"
            );
        }
        let mut unknown = vec![0; MIN_SHARD];
        write_frame_header(&facts(), 3, &mut unknown);
        for bit in [0x08, 0x10, 0x80] {
            unknown[0] = HEVC | bit;
            assert_eq!(
                read_frame(&unknown, MIN_SHARD),
                Err(PacketError::Flags(HEVC | bit))
            );
            assert_eq!(read_flags(&unknown), None);
        }
    }

    #[test]
    fn smallest_rounded_shard() {
        assert_eq!(shard_for(1, 1), MIN_SHARD);
        assert_eq!(shard_for(300, 1), 512);
        assert_eq!(shard_for(491, 1), 512);
        assert_eq!(shard_for(492, 1), 576);
        assert_eq!(shard_for(1131, 1), 1152);
        // One byte more takes two shards, each about half as long: 1153
        // over two, rounded up to 640.
        assert_eq!(data_shards(1132, 1152), 2);
        assert_eq!(shard_for(1132, 2), 640);
        assert_eq!(data_shards(1200, 1152), 2);
        assert_eq!(shard_for(1200, 2), 640);
        // A frame that fills its last shard keeps the full size.
        assert_eq!(shard_for(72 * 1152 - FRAME_HEADER, 72), 1152);
        assert_eq!(shard_for(1323, 1), MAX_SHARD);
        assert_eq!(data_shards(1324, MAX_SHARD), 2);

        // Every length up to five full shards, for the smallest full shard,
        // one step over it, 1024, and the internet's and the LAN's.
        for full in [MIN_SHARD, MIN_SHARD + SHARD_STEP, 1024, 1152, MAX_SHARD] {
            for len in 1..=5 * full {
                let data = data_shards(len, full);
                let shard = shard_for(len, data);
                assert!(shard.is_multiple_of(SHARD_STEP) && (MIN_SHARD..=full).contains(&shard));
                assert_eq!(data_shards(len, shard), data, "{len} bytes, {full}");
                assert!(
                    shard == MIN_SHARD || data_shards(len, shard - SHARD_STEP) > data,
                    "{len} bytes in shards of {full}: {shard} is not the smallest"
                );
                let (frame, cut) = frame_of(len, full);
                assert_eq!(cut, shard);
                let end = FRAME_HEADER + len;
                assert_eq!(read_frame(&frame, shard), Ok((facts(), FRAME_HEADER..end)));
            }
            // Every frame under 492 bytes goes in one shard of the same
            // length, whatever the path.
            for len in 1..=MIN_SHARD - FRAME_HEADER {
                let data = data_shards(len, full);
                assert_eq!((data, shard_for(len, data)), (1, MIN_SHARD));
            }
        }
    }

    #[test]
    fn only_the_packetizers_form_is_read() {
        // 1100 bytes and the header in shards of at most 640: two of 576,
        // with 31 bytes of padding.
        let (good, shard) = frame_of(1100, 640);
        assert_eq!((good.len(), shard), (1152, 576));
        assert!(read_frame(&good, 576).is_ok());
        // 1131 bytes fill both shards exactly.
        assert_eq!(
            read_frame(&frame_of(1131, 640).0, 576),
            Ok((facts(), 21..1152))
        );

        let mut flags = good.clone();
        flags[0] = 0x08;
        assert_eq!(read_frame(&flags, 576), Err(PacketError::Flags(0x08)));
        assert_eq!(read_flags(&flags), None);

        // A length that needs fewer shards, one that needs more, zero, and
        // two that fit in two shards of 512, so 576 would be padding the
        // packetizer never adds.
        for len in [500u32, 1132, 0, 1003, 980] {
            let mut bad = good.clone();
            bad[17..21].copy_from_slice(&len.to_le_bytes());
            assert_eq!(
                read_frame(&bad, 576),
                Err(PacketError::Length {
                    len,
                    data: 2,
                    shard: 576
                })
            );
        }
        let mut huge = good.clone();
        huge[17..21].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            read_frame(&huge, 576),
            Err(PacketError::Length { .. })
        ));

        let mut padding = good.clone();
        padding[1151] = 1;
        assert_eq!(read_frame(&padding, 576), Err(PacketError::Padding));

        // 100 bytes padded out to a whole full-size shard, and in two shards
        // of the smallest length where one carries them.
        for (data, shard) in [(1, 1152), (2, MIN_SHARD)] {
            let mut padded = vec![0; data * shard];
            write_frame_header(&facts(), 100, &mut padded);
            assert_eq!(
                read_frame(&padded, shard),
                Err(PacketError::Length {
                    len: 100,
                    data,
                    shard
                })
            );
        }
    }
}
