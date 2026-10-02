// Voice packets, the payload of Channel::Voice. Every byte here came from a
// friend's PC, which may be compromised, and the Opus frames go on to
// libopus, which is C. So a packet is read field by field, every length
// is checked against Booth's own limits, the Opus frame's first byte has to
// say it is what the header says, and a packet with anything left over is
// refused. Only what passes here reaches the jitter buffer, and only what the
// jitter buffer keeps is ever decoded.
//
// A talker sends Spoken to the host:
//   kind 1, seq u16, captured u64, frame ms u8, flags u8,
//   frame length u8 and the Opus frame,
//   with PREVIOUS: length u8 and the frame before this one,
//   with PADDED: length u8 and that many zero bytes.
// The host sends Relayed to everyone else: kind 2, then the talker's slot u8,
// then the rest as Spoken, with the capture time moved to the host's clock.
// Numbers are little endian. `captured` is microseconds on the sender's ping
// clock (peer::Clock), the time the frame's first sample was captured.

use std::fmt;

use voice::codec::{CodecError, MAX_PACKET, Mode, PacketInfo};

use crate::control::MAX_ROSTER;

const SPOKEN: u8 = 1;
const RELAYED: u8 = 2;

// Redundancy is on. Set on every packet while it is, the first of a spell
// included, which has no frame before it to carry.
const REDUNDANCY: u8 = 1 << 0;
// The frame before this one follows, sent again.
const PREVIOUS: u8 = 1 << 1;
// The talker stopped after this frame.
const LAST: u8 = 1 << 2;
// Zero bytes follow in place of a frame before this one, so that at a
// constant rate the first packet of a spell with redundancy on is no shorter
// than the rest.
const PADDED: u8 = 1 << 3;
// Relayed only: the host's clock offset to the talker was taken over a
// jittery link, so a time worked out from it is only about right.
const ABOUT: u8 = 1 << 4;

const SPOKEN_FLAGS: u8 = REDUNDANCY | PREVIOUS | LAST | PADDED;
const RELAYED_FLAGS: u8 = SPOKEN_FLAGS | ABOUT;

// A relayed capture time of zero: the host had no clock offset for the
// talker, so nobody can tell how long the frame took. The ping clock counts
// from 1970 and is never zero.
const NOT_KNOWN: u64 = 0;

// The longest packet either kind can be, for buffers.
pub(crate) const MAX_VOICE: usize = 2 + 2 + 8 + 1 + 1 + 3 * (1 + MAX_PACKET);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Frame<'a> {
    pub seq: u16,
    // Zero in a Relayed packet when the host could not convert it.
    pub captured: u64,
    pub mode: Mode,
    pub redundancy: bool,
    pub last: bool,
    pub frame: &'a [u8],
    pub previous: Option<&'a [u8]>,
    // Zero bytes standing in for a missing previous frame.
    pub pad: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Relayed<'a> {
    pub slot: u8,
    pub about: bool,
    pub frame: Frame<'a>,
}

impl Relayed<'_> {
    pub(crate) fn captured(&self) -> Option<u64> {
        (self.frame.captured != NOT_KNOWN).then_some(self.frame.captured)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WireError {
    Short,
    Kind(u8),
    FrameMs(u8),
    Flags(u8),
    Length(usize),
    Opus(String),
    // The header says one frame length and the Opus frame another.
    Mismatch { said_ms: u32, frame_ms: u32 },
    Pad,
    Slot(u8),
    Trailing(usize),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::Short => f.write_str("it ends early"),
            WireError::Kind(kind) => write!(f, "kind {kind} is not one this side takes"),
            WireError::FrameMs(ms) => write!(f, "a frame of {ms} ms; Booth sends 5 or 10"),
            WireError::Flags(flags) => write!(f, "flags {flags:#04x} do not go together"),
            WireError::Length(len) => {
                write!(f, "a frame of {len} bytes; Booth sends 1 to {MAX_PACKET}")
            }
            WireError::Opus(why) => write!(f, "the opus frame is not one Booth sends: {why}"),
            WireError::Mismatch { said_ms, frame_ms } => write!(
                f,
                "the header says {said_ms} ms and the opus frame is {frame_ms} ms"
            ),
            WireError::Pad => f.write_str("the padding is not zero bytes"),
            WireError::Slot(slot) => write!(f, "slot {slot} is past the roster"),
            WireError::Trailing(len) => write!(f, "{len} bytes after the end"),
        }
    }
}

impl Frame<'_> {
    pub(crate) fn write_spoken(&self, out: &mut Vec<u8>) {
        out.clear();
        out.push(SPOKEN);
        self.write_body(0, out);
    }

    // The host's copy for everyone else: the same frame, a slot the talker
    // did not choose, and a capture time on the host's clock (None when the
    // host had no offset for the talker).
    pub(crate) fn write_relayed(
        &self,
        slot: u8,
        captured: Option<u64>,
        about: bool,
        out: &mut Vec<u8>,
    ) {
        out.clear();
        out.push(RELAYED);
        out.push(slot);
        let relayed = Frame {
            captured: captured.unwrap_or(NOT_KNOWN),
            ..*self
        };
        relayed.write_body(if about { ABOUT } else { 0 }, out);
    }

    // Callers keep the lengths within MAX_PACKET; past it the frame is cut,
    // which the far side refuses as a frame that is not Opus.
    fn write_body(&self, extra: u8, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.captured.to_le_bytes());
        out.push(self.mode.frame_ms() as u8);
        let mut flags = extra;
        for (on, bit) in [
            (self.redundancy, REDUNDANCY),
            (self.previous.is_some(), PREVIOUS),
            (self.last, LAST),
            (self.previous.is_none() && self.pad > 0, PADDED),
        ] {
            if on {
                flags |= bit;
            }
        }
        out.push(flags);
        put_frame(out, self.frame);
        match self.previous {
            Some(previous) => put_frame(out, previous),
            None if self.pad > 0 => {
                let pad = self.pad.min(MAX_PACKET);
                out.push(pad as u8);
                out.resize(out.len() + pad, 0);
            }
            None => {}
        }
    }
}

fn put_frame(out: &mut Vec<u8>, frame: &[u8]) {
    let frame = &frame[..frame.len().min(MAX_PACKET)];
    out.push(frame.len() as u8);
    out.extend_from_slice(frame);
}

// What a host takes from a client.
pub(crate) fn read_spoken(packet: &[u8]) -> Result<Frame<'_>, WireError> {
    let mut r = Reader(packet);
    match r.u8()? {
        SPOKEN => {}
        kind => return Err(WireError::Kind(kind)),
    }
    let (frame, _) = read_body(&mut r, SPOKEN_FLAGS)?;
    r.end()?;
    Ok(frame)
}

// What a client takes from the host.
pub(crate) fn read_relayed(packet: &[u8]) -> Result<Relayed<'_>, WireError> {
    let mut r = Reader(packet);
    match r.u8()? {
        RELAYED => {}
        kind => return Err(WireError::Kind(kind)),
    }
    let slot = r.u8()?;
    if usize::from(slot) >= MAX_ROSTER {
        return Err(WireError::Slot(slot));
    }
    let (frame, flags) = read_body(&mut r, RELAYED_FLAGS)?;
    r.end()?;
    Ok(Relayed {
        slot,
        about: flags & ABOUT != 0,
        frame,
    })
}

fn read_body<'a>(r: &mut Reader<'a>, allowed: u8) -> Result<(Frame<'a>, u8), WireError> {
    let seq = u16::from_le_bytes(r.array()?);
    let captured = u64::from_le_bytes(r.array()?);
    let mode = match r.u8()? {
        5 => Mode::LowDelay,
        10 => Mode::Repair,
        ms => return Err(WireError::FrameMs(ms)),
    };
    let flags = r.u8()?;
    let together = flags & !allowed == 0
        && (flags & PREVIOUS == 0 || flags & REDUNDANCY != 0)
        && flags & (PREVIOUS | PADDED) != (PREVIOUS | PADDED);
    if !together {
        return Err(WireError::Flags(flags));
    }
    let frame = opus_frame(r)?;
    let info = PacketInfo::read(frame).map_err(opus_error)?;
    if info.mode() != mode {
        return Err(WireError::Mismatch {
            said_ms: mode.frame_ms(),
            frame_ms: info.mode().frame_ms(),
        });
    }
    // Either size: across a switch between modes the frame before is the
    // other one.
    let previous = if flags & PREVIOUS != 0 {
        let previous = opus_frame(r)?;
        PacketInfo::read(previous).map_err(opus_error)?;
        Some(previous)
    } else {
        None
    };
    let pad = if flags & PADDED != 0 {
        let pad = bytes(r)?;
        if pad.iter().any(|&byte| byte != 0) {
            return Err(WireError::Pad);
        }
        pad.len()
    } else {
        0
    };
    let frame = Frame {
        seq,
        captured,
        mode,
        redundancy: flags & REDUNDANCY != 0,
        last: flags & LAST != 0,
        frame,
        previous,
        pad,
    };
    Ok((frame, flags))
}

// A length byte, then that many bytes: at least one, at most MAX_PACKET.
fn bytes<'a>(r: &mut Reader<'a>) -> Result<&'a [u8], WireError> {
    let len = usize::from(r.u8()?);
    if len == 0 || len > MAX_PACKET {
        return Err(WireError::Length(len));
    }
    r.take(len)
}

fn opus_frame<'a>(r: &mut Reader<'a>) -> Result<&'a [u8], WireError> {
    bytes(r)
}

fn opus_error(err: CodecError) -> WireError {
    WireError::Opus(err.to_string())
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn u8(&mut self) -> Result<u8, WireError> {
        let (&first, rest) = self.0.split_first().ok_or(WireError::Short)?;
        self.0 = rest;
        Ok(first)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], WireError> {
        if len > self.0.len() {
            return Err(WireError::Short);
        }
        let (taken, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(taken)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
        let taken = self.take(N)?;
        taken.try_into().map_err(|_| WireError::Short)
    }

    fn end(&self) -> Result<(), WireError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(WireError::Trailing(self.0.len()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // First bytes (RFC 6716, 3.1) of what Booth sends, with no encoder in
    // the way: a mono CELT 5 ms frame, a mono CELT 10 ms frame and a mono
    // SILK 10 ms frame.
    const CELT_5: u8 = 29 << 3;
    const CELT_10: u8 = 30 << 3;
    const SILK_10: u8 = 8 << 3;

    fn opus(first: u8, len: usize) -> Vec<u8> {
        let mut frame = vec![first];
        frame.extend((1..len).map(|i| (i * 37) as u8));
        frame
    }

    fn frame<'a>(bytes: &'a [u8], previous: Option<&'a [u8]>) -> Frame<'a> {
        Frame {
            seq: 65_530,
            captured: 1_790_284_323_456_789,
            mode: PacketInfo::read(bytes).unwrap().mode(),
            redundancy: previous.is_some(),
            last: false,
            frame: bytes,
            previous,
            pad: 0,
        }
    }

    fn spoken(frame: &Frame) -> Vec<u8> {
        let mut out = Vec::new();
        frame.write_spoken(&mut out);
        out
    }

    fn relayed(frame: &Frame, slot: u8, captured: Option<u64>, about: bool) -> Vec<u8> {
        let mut out = Vec::new();
        frame.write_relayed(slot, captured, about, &mut out);
        out
    }

    #[test]
    fn every_shape_booth_sends_round_trips() {
        let five = opus(CELT_5, 20);
        let ten = opus(CELT_10, 40);
        let silk = opus(SILK_10, 40);
        let shapes = [
            frame(&five, None),
            frame(&five, Some(&five)),
            frame(&ten, None),
            frame(&silk, Some(&ten)),
            // Across a switch the frame before is the other size.
            frame(&ten, Some(&five)),
            Frame {
                last: true,
                ..frame(&five, None)
            },
            Frame {
                redundancy: true,
                pad: 21,
                ..frame(&five, None)
            },
            Frame {
                redundancy: true,
                ..frame(&five, None)
            },
        ];
        for sent in shapes {
            assert_eq!(read_spoken(&spoken(&sent)), Ok(sent), "{sent:?}");
            for (captured, about) in [(Some(12_345), false), (None, true)] {
                let packet = relayed(&sent, 7, captured, about);
                let got = read_relayed(&packet).unwrap();
                assert_eq!((got.slot, got.about, got.captured()), (7, about, captured));
                assert_eq!(
                    got.frame,
                    Frame {
                        captured: captured.unwrap_or(0),
                        ..sent
                    }
                );
            }
        }
    }

    // At a constant rate with redundancy on, a spell's first packet carries
    // padding the size of a frame and is as long as the rest.
    #[test]
    fn padding_evens_first_packet() {
        let five = opus(CELT_5, 20);
        let first = Frame {
            redundancy: true,
            pad: 20,
            ..frame(&five, None)
        };
        let next = frame(&five, Some(&five));
        assert_eq!(spoken(&first).len(), spoken(&next).len());
        assert_eq!(
            relayed(&first, 1, Some(9), false).len(),
            relayed(&next, 1, Some(9), false).len()
        );
        assert_eq!(spoken(&next).len(), 13 + 21 + 21);
    }

    #[test]
    fn each_side_takes_only_its_own_kind() {
        let five = opus(CELT_5, 20);
        let sent = frame(&five, None);
        assert_eq!(read_relayed(&spoken(&sent)), Err(WireError::Kind(SPOKEN)));
        assert_eq!(
            read_spoken(&relayed(&sent, 1, None, false)),
            Err(WireError::Kind(RELAYED))
        );
    }

    #[test]
    fn a_bad_field_is_refused_and_says_which() {
        let five = opus(CELT_5, 20);
        let good = spoken(&frame(&five, Some(&five)));
        let with = |at: usize, byte: u8| {
            let mut bad = good.clone();
            bad[at] = byte;
            read_spoken(&bad).map(|_| ())
        };
        // Offsets: kind 0, seq 1, captured 3, ms 11, flags 12, length 13,
        // frame 14, previous length 34.
        assert_eq!(with(0, 9), Err(WireError::Kind(9)));
        assert_eq!(with(11, 20), Err(WireError::FrameMs(20)));
        assert_eq!(
            with(11, 10),
            Err(WireError::Mismatch {
                said_ms: 10,
                frame_ms: 5
            })
        );
        assert_eq!(with(12, 0x40), Err(WireError::Flags(0x40)));
        assert_eq!(
            with(12, ABOUT | REDUNDANCY | PREVIOUS),
            Err(WireError::Flags(0x13))
        );
        // A copy with redundancy said to be off.
        assert_eq!(with(12, PREVIOUS), Err(WireError::Flags(PREVIOUS)));
        assert_eq!(
            with(12, REDUNDANCY | PREVIOUS | PADDED),
            Err(WireError::Flags(0x0b))
        );
        assert_eq!(with(13, 0), Err(WireError::Length(0)));
        assert_eq!(with(13, 200), Err(WireError::Length(200)));
        // A frame length that runs into the copy after it.
        assert_eq!(with(13, 30), Err(WireError::Short));
        // Stereo, two frames, a 20 ms frame.
        assert!(matches!(with(14, CELT_5 | 0x04), Err(WireError::Opus(_))));
        assert!(matches!(with(14, CELT_5 | 0x01), Err(WireError::Opus(_))));
        assert!(matches!(with(14, 31 << 3), Err(WireError::Opus(_))));
        assert!(matches!(with(35, 0x04 | CELT_5), Err(WireError::Opus(_))));
        assert_eq!(with(34, 21), Err(WireError::Short));

        let mut trailing = good.clone();
        trailing.push(0);
        assert_eq!(read_spoken(&trailing), Err(WireError::Trailing(1)));
        for cut in 0..good.len() {
            assert!(read_spoken(&good[..cut]).is_err(), "cut at {cut}");
        }

        let padded = spoken(&Frame {
            redundancy: true,
            pad: 20,
            ..frame(&five, None)
        });
        let mut dirty = padded.clone();
        let last = dirty.len() - 1;
        dirty[last] = 1;
        assert_eq!(read_spoken(&dirty), Err(WireError::Pad));

        let mut beyond = relayed(&frame(&five, None), 1, None, false);
        beyond[1] = MAX_ROSTER as u8;
        assert_eq!(
            read_relayed(&beyond),
            Err(WireError::Slot(MAX_ROSTER as u8))
        );
    }

    // A friend's program can send anything at all, as long as it likes.
    #[test]
    fn huge_and_garbage_refused() {
        let mut huge = vec![SPOKEN, 1, 0, 1, 2, 3, 4, 5, 6, 7, 8, 5, 0, 255];
        huge.extend(std::iter::repeat_n(CELT_5, 60_000));
        assert_eq!(read_spoken(&huge), Err(WireError::Length(255)));
        let mut long_frame = vec![SPOKEN, 1, 0, 1, 2, 3, 4, 5, 6, 7, 8, 5, 0, 129];
        long_frame.extend(std::iter::repeat_n(CELT_5, 129));
        assert_eq!(read_spoken(&long_frame), Err(WireError::Length(129)));
        // A frame length that fits, over bytes that are not an Opus frame
        // Booth would ever send.
        let mut garbage = vec![SPOKEN, 1, 0, 1, 2, 3, 4, 5, 6, 7, 8, 5, 0, 20];
        garbage.extend(std::iter::repeat_n(0xff, 20));
        assert!(matches!(read_spoken(&garbage), Err(WireError::Opus(_))));
        assert_eq!(read_spoken(&[]), Err(WireError::Short));
        assert_eq!(read_relayed(&[RELAYED]), Err(WireError::Short));
    }

    // xorshift64*, seeded, so a failure shows up the same way every run.
    struct Random(u64);

    impl Random {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    // Whatever gets through is something Booth could have sent, and writing
    // it again gives the same bytes back.
    fn check_accepted(packet: &[u8]) {
        if let Ok(frame) = read_spoken(packet) {
            assert!(frame.frame.len() <= MAX_PACKET);
            assert_eq!(PacketInfo::read(frame.frame).unwrap().mode(), frame.mode);
            assert_eq!(spoken(&frame), packet);
        }
        if let Ok(heard) = read_relayed(packet) {
            assert!(usize::from(heard.slot) < MAX_ROSTER);
            let written = relayed(&heard.frame, heard.slot, heard.captured(), heard.about);
            assert_eq!(written, packet);
        }
    }

    #[test]
    fn edited_packets_never_panic() {
        let five = opus(CELT_5, 20);
        let ten = opus(SILK_10, 40);
        let seeds = [
            spoken(&frame(&five, Some(&five))),
            spoken(&Frame {
                redundancy: true,
                pad: 20,
                last: true,
                ..frame(&five, None)
            }),
            relayed(&frame(&ten, Some(&five)), 3, Some(77), true),
            relayed(&frame(&five, None), 0, None, false),
        ];
        let mut random = Random(0x9E37_79B9_7F4A_7C15);
        for _ in 0..50_000 {
            let mut packet = seeds[random.below(seeds.len())].clone();
            for _ in 0..1 + random.below(4) {
                let at = random.below(packet.len() + 1);
                match random.below(3) {
                    0 if at < packet.len() => packet[at] = random.next() as u8,
                    1 => packet.insert(at, random.next() as u8),
                    _ if at < packet.len() => {
                        packet.remove(at);
                    }
                    _ => {}
                }
            }
            check_accepted(&packet);
        }
    }

    #[test]
    fn random_bytes_never_panic() {
        let mut random = Random(0xD1B5_4A32_D192_ED03);
        for _ in 0..50_000 {
            let len = random.below(300);
            let mut packet: Vec<u8> = (0..len).map(|_| random.next() as u8).collect();
            if let Some(kind) = packet.first_mut() {
                *kind = [SPOKEN, RELAYED, *kind][random.below(3)];
            }
            check_accepted(&packet);
        }
    }

    fn any_opus() -> impl Strategy<Value = Vec<u8>> {
        (
            prop::sample::select(vec![CELT_5, CELT_10, SILK_10, 12 << 3, 17 << 3]),
            prop::collection::vec(any::<u8>(), 0..MAX_PACKET),
        )
            .prop_map(|(first, rest)| {
                let mut frame = vec![first];
                frame.extend(rest);
                frame
            })
    }

    proptest! {
        #[test]
        fn any_frame_booth_could_send_round_trips(
            seq in any::<u16>(),
            captured in 1u64..,
            bytes in any_opus(),
            previous in prop::option::of(any_opus()),
            last in any::<bool>(),
            pad in 0usize..=MAX_PACKET,
            slot in 0u8..MAX_ROSTER as u8,
            about in any::<bool>(),
        ) {
            let sent = Frame {
                seq,
                captured,
                mode: PacketInfo::read(&bytes).unwrap().mode(),
                redundancy: previous.is_some() || pad > 0,
                last,
                frame: &bytes,
                previous: previous.as_deref(),
                pad: if previous.is_some() { 0 } else { pad },
            };
            let packet = spoken(&sent);
            prop_assert!(packet.len() <= MAX_VOICE);
            prop_assert_eq!(read_spoken(&packet), Ok(sent));
            let packet = relayed(&sent, slot, Some(captured), about);
            prop_assert!(packet.len() <= MAX_VOICE);
            let got = read_relayed(&packet).unwrap();
            prop_assert_eq!((got.slot, got.about, got.frame), (slot, about, sent));
        }

        #[test]
        fn any_bytes_parse_or_are_refused(packet in prop::collection::vec(any::<u8>(), 0..400)) {
            check_accepted(&packet);
        }
    }
}
