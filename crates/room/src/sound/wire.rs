// PC sound packets, the payload of Channel::Sound. Every byte here came from
// a friend's PC, which may be compromised, and the Opus frames go on to
// libopus, which is C. So a packet is read field by field, every length is
// checked against Booth's own limits, the Opus frame has to be one Booth
// sends (voice::stereo::FrameInfo), and a packet with anything left over is
// refused. Only what passes here reaches a buffer, and only what a buffer
// keeps is ever decoded.
//
//   offset size
//   0      1    kind: 1 SENT, the sharer to the host; 2 RELAYED, the host on
//   1      1    slot: 0 in SENT; in RELAYED the sharer's slot
//   2      4    share u32, the share this sound goes with, never 0
//   6      2    seq u16
//   8      8    captured u64: microseconds on the sharer's ping clock
//                (peer::Clock) when the frame's first sample was captured,
//                never 0, left alone by the host as video's capture time is
//   16     1    flags: LAST, FIRST; every other bit 0
//   17     1    length of the Opus frame, 1 to voice::stereo::MAX_PACKET
//   18     n    the Opus frame
//
// Little endian; the same two-byte prefix as video's, so the host turns
// SENT into RELAYED and writes the slot in place, touching nothing after it.

use std::fmt;

use voice::stereo::{FrameInfo, MAX_PACKET};

use crate::control::MAX_ROSTER;

const SENT: u8 = 1;
const RELAYED: u8 = 2;

// The stream stops after this frame.
const LAST: u8 = 1 << 0;
// The stream starts again here, from a reset encoder.
const FIRST: u8 = 1 << 1;
const FLAGS: u8 = LAST | FIRST;

const HEADER: usize = 18;
pub(crate) const MAX_SOUND: usize = HEADER + MAX_PACKET;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SoundFrame<'a> {
    // 0 in a SENT packet.
    pub slot: u8,
    pub share: u32,
    pub seq: u16,
    pub captured: u64,
    pub first: bool,
    pub last: bool,
    pub frame: &'a [u8],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SoundRefused {
    TooLong(usize),
    Short,
    Kind(u8),
    Slot(u8),
    Share,
    Time,
    Flags(u8),
    Length(usize),
    Opus(String),
    Trailing(usize),
}

impl fmt::Display for SoundRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SoundRefused::TooLong(len) => {
                write!(f, "{len} bytes, more than the {MAX_SOUND} Booth sends")
            }
            SoundRefused::Short => f.write_str("it ends early"),
            SoundRefused::Kind(kind) => write!(f, "kind {kind} is not one this side takes"),
            SoundRefused::Slot(slot) => write!(f, "slot {slot} is not one this kind carries"),
            SoundRefused::Share => f.write_str("it names share 0"),
            SoundRefused::Time => f.write_str("it has no capture time"),
            SoundRefused::Flags(flags) => write!(f, "flags {flags:#04x} are not ones Booth sets"),
            SoundRefused::Length(len) => {
                write!(f, "a frame of {len} bytes; Booth sends 1 to {MAX_PACKET}")
            }
            SoundRefused::Opus(why) => write!(f, "the opus frame is not one Booth sends: {why}"),
            SoundRefused::Trailing(len) => write!(f, "{len} bytes after the end"),
        }
    }
}

#[cfg_attr(not(test), expect(dead_code, reason = "nothing sends PC sound yet"))]
impl SoundFrame<'_> {
    pub(crate) fn write_sent(&self, out: &mut Vec<u8>) {
        self.write(SENT, 0, out);
    }

    pub(crate) fn write_relayed(&self, slot: u8, out: &mut Vec<u8>) {
        self.write(RELAYED, slot, out);
    }

    fn write(&self, kind: u8, slot: u8, out: &mut Vec<u8>) {
        out.clear();
        out.push(kind);
        out.push(slot);
        out.extend_from_slice(&self.share.to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.captured.to_le_bytes());
        let mut flags = 0;
        if self.last {
            flags |= LAST;
        }
        if self.first {
            flags |= FIRST;
        }
        out.push(flags);
        out.push(self.frame.len() as u8);
        out.extend_from_slice(self.frame);
    }
}

pub(crate) fn read_sent(packet: &[u8]) -> Result<SoundFrame<'_>, SoundRefused> {
    read(packet, SENT)
}

pub(crate) fn read_relayed(packet: &[u8]) -> Result<SoundFrame<'_>, SoundRefused> {
    read(packet, RELAYED)
}

// The host's copy of a SENT packet read just before, in place: the kind and
// the sharer's slot, nothing after them.
pub(crate) fn relay_in_place(packet: &mut [u8], slot: u8) {
    packet[0] = RELAYED;
    packet[1] = slot;
}

fn read(packet: &[u8], kind: u8) -> Result<SoundFrame<'_>, SoundRefused> {
    if packet.len() > MAX_SOUND {
        return Err(SoundRefused::TooLong(packet.len()));
    }
    let mut r = Reader(packet);
    let got = r.u8()?;
    if got != kind {
        return Err(SoundRefused::Kind(got));
    }
    let slot = r.u8()?;
    let slot_fits = match kind {
        SENT => slot == 0,
        _ => usize::from(slot) < MAX_ROSTER,
    };
    if !slot_fits {
        return Err(SoundRefused::Slot(slot));
    }
    let share = r.u32()?;
    if share == 0 {
        return Err(SoundRefused::Share);
    }
    let seq = r.u16()?;
    let captured = r.u64()?;
    if captured == 0 {
        return Err(SoundRefused::Time);
    }
    let flags = r.u8()?;
    if flags & !FLAGS != 0 {
        return Err(SoundRefused::Flags(flags));
    }
    let len = usize::from(r.u8()?);
    if len == 0 || len > MAX_PACKET {
        return Err(SoundRefused::Length(len));
    }
    let frame = r.take(len)?;
    FrameInfo::read(frame).map_err(|err| SoundRefused::Opus(err.to_string()))?;
    if !r.0.is_empty() {
        return Err(SoundRefused::Trailing(r.0.len()));
    }
    Ok(SoundFrame {
        slot,
        share,
        seq,
        captured,
        first: flags & FIRST != 0,
        last: flags & LAST != 0,
        frame,
    })
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], SoundRefused> {
        if self.0.len() < len {
            return Err(SoundRefused::Short);
        }
        let (taken, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(taken)
    }

    fn u8(&mut self) -> Result<u8, SoundRefused> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, SoundRefused> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, SoundRefused> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, SoundRefused> {
        let b = self.take(8)?;
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(b);
        Ok(u64::from_le_bytes(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use voice::stereo::{Encoder, FRAME, PACKET_BYTES};

    fn opus_frame() -> Vec<u8> {
        let mut encoder = Encoder::new().unwrap();
        let mut out = [0u8; MAX_PACKET];
        let len = encoder.encode(&[0.1; FRAME * 2], &mut out).unwrap();
        out[..len].to_vec()
    }

    fn sample(frame: &[u8]) -> SoundFrame<'_> {
        SoundFrame {
            slot: 0,
            share: 7,
            seq: 513,
            captured: 1_700_000_000_123_456,
            first: true,
            last: false,
            frame,
        }
    }

    #[test]
    fn every_shape_reads_back() {
        let frame = opus_frame();
        assert_eq!(frame.len(), PACKET_BYTES);
        // Opus's own padding, code 3, and a frame cut short of 120.
        let mut padded = frame.clone();
        padded[0] |= 0x03;
        padded.insert(1, 1);
        let short = opus_frame()[..1].to_vec();
        for frame in [&frame[..], &padded[..], &short[..]] {
            for (first, last) in [(false, false), (true, false), (false, true), (true, true)] {
                let sent = SoundFrame {
                    first,
                    last,
                    ..sample(frame)
                };
                let mut out = Vec::new();
                sent.write_sent(&mut out);
                assert_eq!(read_sent(&out), Ok(sent));
                assert!(read_relayed(&out).is_err());
                let mut relayed = Vec::new();
                sent.write_relayed(5, &mut relayed);
                assert_eq!(read_relayed(&relayed), Ok(SoundFrame { slot: 5, ..sent }));
                assert!(read_sent(&relayed).is_err());
                // The host's flip in place makes the same bytes.
                relay_in_place(&mut out, 5);
                assert_eq!(out, relayed);
            }
        }
    }

    // Each field refused for what it is: kind at 0, slot 1, share 2, seq 6,
    // captured 8, flags 16, length 17, the frame at 18.
    #[test]
    fn each_bad_field_says_which() {
        let frame = opus_frame();
        let mut good = Vec::new();
        sample(&frame).write_sent(&mut good);
        let bent = |at: usize, value: u8| {
            let mut packet = good.clone();
            packet[at] = value;
            read_sent(&packet).unwrap_err()
        };
        assert_eq!(bent(0, 2), SoundRefused::Kind(2));
        assert_eq!(bent(1, 3), SoundRefused::Slot(3));
        let mut relayed = Vec::new();
        sample(&frame).write_relayed(MAX_ROSTER as u8, &mut relayed);
        assert_eq!(
            read_relayed(&relayed),
            Err(SoundRefused::Slot(MAX_ROSTER as u8))
        );
        let mut zero_share = good.clone();
        zero_share[2..6].fill(0);
        assert_eq!(read_sent(&zero_share), Err(SoundRefused::Share));
        let mut zero_time = good.clone();
        zero_time[8..16].fill(0);
        assert_eq!(read_sent(&zero_time), Err(SoundRefused::Time));
        assert_eq!(bent(16, 0x04), SoundRefused::Flags(0x04));
        assert_eq!(bent(17, 0), SoundRefused::Length(0));
        assert_eq!(bent(17, 161), SoundRefused::Length(161));
        assert!(matches!(bent(18, frame[0] & !0x04), SoundRefused::Opus(_)));
        assert_eq!(read_sent(&good[..good.len() - 1]), Err(SoundRefused::Short));
        let mut long = good.clone();
        long.push(0);
        assert_eq!(read_sent(&long), Err(SoundRefused::Trailing(1)));
        assert_eq!(
            read_sent(&vec![1; 60_000]),
            Err(SoundRefused::TooLong(60_000))
        );
        // Cut anywhere, refused.
        for len in 0..good.len() {
            assert!(read_sent(&good[..len]).is_err(), "{len} bytes");
        }
    }

    // Mono, 5 and 20 ms, two frames, SILK and hybrid: refused before
    // libopus sees them.
    #[test]
    fn frames_booth_does_not_send_are_refused() {
        let frame = opus_frame();
        for toc in [
            frame[0] & !0x04,
            (29 << 3) | 0x04,
            (31 << 3) | 0x04,
            frame[0] | 0x01,
            (1 << 3) | 0x04,
            (13 << 3) | 0x04,
        ] {
            let mut bent = frame.clone();
            bent[0] = toc;
            let mut out = Vec::new();
            sample(&bent).write_sent(&mut out);
            assert!(
                matches!(read_sent(&out), Err(SoundRefused::Opus(_))),
                "{toc:#04x}"
            );
        }
    }

    // Whatever comes is read or refused, never a panic, and what is read
    // writes back to the same bytes.
    #[test]
    fn any_bytes_are_read_or_refused() {
        let frame = opus_frame();
        let mut good = Vec::new();
        sample(&frame).write_sent(&mut good);
        let mut seed = 0x2545_f491u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for _ in 0..50_000 {
            let mut packet = good.clone();
            for _ in 0..1 + next() % 4 {
                let at = next() as usize % packet.len();
                packet[at] = next() as u8;
            }
            if next() % 8 == 0 {
                packet.truncate(next() as usize % packet.len());
            }
            if let Ok(read) = read_sent(&packet) {
                let mut again = Vec::new();
                read.write_sent(&mut again);
                assert_eq!(again, packet);
            }
            let random: Vec<u8> = (0..next() % 200).map(|_| next() as u8).collect();
            let _ = read_relayed(&random);
        }
    }
}
