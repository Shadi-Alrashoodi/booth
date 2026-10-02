// The sharer's side: one access unit into packets, data shards first, then
// the Reed-Solomon parity. Every packet of a frame is the same length, the
// last data shard padded with zeros, so the parity can stand in for any of
// them. A frame takes as many data shards as it needs at the largest shard
// the payload allows, and then the shortest shard in whole steps of 64
// bytes, never under 512, that still carries it in that many (wire.rs). A
// small frame costs 512 bytes a shard, a bigger one about its own size, and
// a big one keeps its packet count.
//
// Allocation: the packet buffer grows to the most packet bytes a frame has
// needed, and the encoder's work space to the most its shard counts times
// its shard length have needed. Both are reused, so a frame that needs no
// more than one before it allocates nothing. A smaller frame can need more:
// 1132 bytes go out in two data packets of 650, but 1131 in one of 1162. A
// share starts with an IDR of many shards, which needs more than any
// one-shard frame after it.

use std::fmt;

use reed_solomon_simd::ReedSolomonEncoder;

use super::loss::parity_for_loss;
use super::wire::{
    FRAME_HEADER, FrameFacts, HEADER, MAX_DATA, MAX_SHARD, MIN_SHARD, SHARD_STEP, data_shards,
    shard_for, write_frame_header, write_header,
};

pub struct Packetizer {
    largest_shard: usize,
    // Every packet of the current frame, back to back.
    packets: Vec<u8>,
    encoder: Option<ReedSolomonEncoder>,
}

impl Packetizer {
    // `payload` is the most bytes one packet may have: what is left of a
    // datagram after the session's overhead, the channel byte and the room's
    // prefix. The largest shard is what that leaves in whole steps, so every
    // shard is a multiple of SHARD_STEP; the rest goes unused, 3 bytes of a
    // 1200-byte datagram and 11 of a 1400-byte one.
    pub fn new(payload: usize) -> Result<Packetizer, PacketizeError> {
        let largest_shard = payload.saturating_sub(HEADER) / SHARD_STEP * SHARD_STEP;
        if !(MIN_SHARD..=MAX_SHARD).contains(&largest_shard) {
            return Err(PacketizeError::Payload(payload));
        }
        Ok(Packetizer {
            largest_shard,
            packets: Vec::new(),
            encoder: None,
        })
    }

    pub fn largest_shard(&self) -> usize {
        self.largest_shard
    }

    pub fn largest_access_unit(&self) -> usize {
        usize::from(MAX_DATA) * self.largest_shard - FRAME_HEADER
    }

    // Each frame's parity by parity_for_loss, from the viewers' loss.
    pub fn packetize_for_loss(
        &mut self,
        facts: &FrameFacts,
        access_unit: &[u8],
        loss_percent: Option<f32>,
    ) -> Result<Packets<'_>, PacketizeError> {
        self.cut(facts, access_unit, |data| {
            parity_for_loss(data, loss_percent)
        })
    }

    // A fixed percentage of the frame's data shards in parity, by
    // parity_count, whatever the loss.
    pub fn packetize(
        &mut self,
        facts: &FrameFacts,
        access_unit: &[u8],
        parity_percent: u32,
    ) -> Result<Packets<'_>, PacketizeError> {
        self.cut(facts, access_unit, |data| {
            parity_count(data, parity_percent)
        })
    }

    fn cut(
        &mut self,
        facts: &FrameFacts,
        access_unit: &[u8],
        parity_for: impl FnOnce(u16) -> u16,
    ) -> Result<Packets<'_>, PacketizeError> {
        if access_unit.is_empty() {
            return Err(PacketizeError::Empty);
        }
        let largest = self.largest_access_unit();
        if access_unit.len() > largest {
            return Err(PacketizeError::TooBig {
                len: access_unit.len(),
                largest,
            });
        }
        let data = data_shards(access_unit.len(), self.largest_shard);
        let shard = shard_for(access_unit.len(), data);
        let stride = HEADER + shard;
        let data = data as u16;
        let parity = parity_for(data);
        let total = usize::from(data) + usize::from(parity);

        self.packets.clear();
        self.packets.reserve(total * stride);
        for index in 0..total as u16 {
            write_header(facts.number, index, data, parity, &mut self.packets);
            self.packets.resize(self.packets.len() + shard, 0);
        }

        // The frame header and the access unit run across the data shards,
        // skipping each packet's header.
        write_frame_header(
            facts,
            access_unit.len() as u32,
            &mut self.packets[HEADER..HEADER + FRAME_HEADER],
        );
        let mut rest = access_unit;
        for index in 0..usize::from(data) {
            let end = index * stride + HEADER + shard;
            let start = end - shard + if index == 0 { FRAME_HEADER } else { 0 };
            let take = rest.len().min(end - start);
            self.packets[start..start + take].copy_from_slice(&rest[..take]);
            rest = &rest[take..];
        }

        let encoder = match self.encoder.as_mut() {
            Some(encoder) => {
                encoder.reset(usize::from(data), usize::from(parity), shard)?;
                encoder
            }
            None => self.encoder.insert(ReedSolomonEncoder::new(
                usize::from(data),
                usize::from(parity),
                shard,
            )?),
        };
        for index in 0..usize::from(data) {
            let start = index * stride + HEADER;
            encoder.add_original_shard(&self.packets[start..start + shard])?;
        }
        let result = encoder.encode()?;
        for (nth, recovery) in result.recovery_iter().enumerate() {
            let start = (usize::from(data) + nth) * stride + HEADER;
            self.packets[start..start + shard].copy_from_slice(recovery);
        }

        Ok(Packets {
            bytes: &self.packets,
            stride,
            data,
            parity,
        })
    }
}

impl fmt::Debug for Packetizer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Packetizer")
            .field("largest_shard", &self.largest_shard)
            .finish()
    }
}

// Parity shards for a frame of `data` data shards: the percentage of the
// data count, rounded up, at least one and at most as many as the data. A
// one-shard frame therefore always gets one parity shard, a copy's worth.
pub fn parity_count(data: u16, percent: u32) -> u16 {
    let percent = percent.min(100);
    let count = (u32::from(data) * percent).div_ceil(100);
    count.clamp(1, u32::from(data.max(1))) as u16
}

// One frame's packets, in the order they should go out.
#[derive(Clone, Copy)]
pub struct Packets<'a> {
    bytes: &'a [u8],
    stride: usize,
    data: u16,
    parity: u16,
}

impl<'a> Packets<'a> {
    pub fn len(&self) -> usize {
        usize::from(self.data) + usize::from(self.parity)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn data(&self) -> u16 {
        self.data
    }

    pub fn parity(&self) -> u16 {
        self.parity
    }

    // Every packet of the frame is this long.
    pub fn packet_len(&self) -> usize {
        self.stride
    }

    pub fn get(&self, index: usize) -> Option<&'a [u8]> {
        let start = index.checked_mul(self.stride)?;
        self.bytes.get(start..start.checked_add(self.stride)?)
    }

    pub fn iter(&self) -> std::slice::ChunksExact<'a, u8> {
        self.bytes.chunks_exact(self.stride)
    }
}

impl<'a> IntoIterator for Packets<'a> {
    type Item = &'a [u8];
    type IntoIter = std::slice::ChunksExact<'a, u8>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl fmt::Debug for Packets<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Packets")
            .field("data", &self.data)
            .field("parity", &self.parity)
            .field("packet_len", &self.stride)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PacketizeError {
    Payload(usize),
    Empty,
    TooBig { len: usize, largest: usize },
    Parity(String),
}

impl From<reed_solomon_simd::Error> for PacketizeError {
    fn from(err: reed_solomon_simd::Error) -> PacketizeError {
        PacketizeError::Parity(err.to_string())
    }
}

impl fmt::Display for PacketizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PacketizeError::Payload(payload) => write!(
                f,
                "a video packet of {payload} bytes; Booth's packets hold a {HEADER}-byte header \
                 and a shard of {MIN_SHARD} to {MAX_SHARD} bytes in steps of {SHARD_STEP}"
            ),
            PacketizeError::Empty => f.write_str("the encoder gave an empty access unit"),
            PacketizeError::TooBig { len, largest } => write!(
                f,
                "an access unit of {len} bytes; one frame carries at most {largest}"
            ),
            PacketizeError::Parity(why) => write!(f, "could not compute the parity: {why}"),
        }
    }
}

impl std::error::Error for PacketizeError {}
