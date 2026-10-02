// Input packets, the payload of Channel::Input.
// Every byte came from a friend's PC and ends up as a key press or a click
// on someone's desktop, so a packet is read field by field against what
// Booth sends, and one with anything left over is refused, the way
// screen/wire.rs reads video.
//
// A controller sends kind SENT with slot 0 to the host; the host sends
// RELAYED with the controller's slot to the sharer, with the capture time
// moved to its own clock, as it does for voice:
//   kind u8, slot u8, seq u32, captured u64, flags u8,
//   buttons u8, keys 64 bytes, count u8, then `count` events, each a tag:
//     KEY     code u8, bits u8 (DOWN, E0)
//     MOVE    dx i16, dy i16
//     AT      x u16, y u16
//     BUTTON  button u8, down u8
//     WHEEL   delta i16
//     HWHEEL  delta i16
// Numbers are little endian. `captured` is microseconds on the sender's ping
// clock (peer::Clock) for the oldest event in the packet; a relayed zero says
// the host had no clock offset for the controller. `buttons` and `keys` are
// everything the controller holds once the events are done: a bit per mouse
// button, and a bit per key at code + 256 for the E0 ones.

use std::fmt;

use super::{Button, Held, InputEvent, ScanCode};
use crate::control::MAX_ROSTER;

pub(crate) const SENT: u8 = 1;
pub(crate) const RELAYED: u8 = 2;

// A burst the capture feed hands over at once, as a chord or a macro gives,
// fits one packet; more goes in the next one, a moment later.
pub const MAX_EVENTS: usize = 32;

const SLOT_AT: usize = 1;
const SEQ_AT: usize = 2;
const CAPTURED_AT: usize = 6;
const FLAGS_AT: usize = 14;
const BUTTONS_AT: usize = 15;
const KEYS_AT: usize = 16;
pub(crate) const KEY_BYTES: usize = 64;
const COUNT_AT: usize = KEYS_AT + KEY_BYTES;
const HEAD: usize = COUNT_AT + 1;
const LONGEST_EVENT: usize = 5;
pub(crate) const MAX_INPUT: usize = HEAD + MAX_EVENTS * LONGEST_EVENT;

// Relayed only: the host's clock offset to the controller was taken over a
// jittery link, so a time worked out from it is only about right.
const ABOUT: u8 = 1 << 0;

const KEY: u8 = 1;
const MOVE: u8 = 2;
const AT: u8 = 3;
const BUTTON: u8 = 4;
const WHEEL: u8 = 5;
const HWHEEL: u8 = 6;

const DOWN: u8 = 1 << 0;
const E0: u8 = 1 << 1;

const BUTTONS: u8 = (1 << 5) - 1;

// The capture time a relayed packet carries when the host could not move it
// onto its own clock. The ping clock counts from 1970 and is never zero.
const NOT_KNOWN: u64 = 0;

// Why is written to the log, so the ones about a key or a button carry no
// value: a packet refused for one bit this build does not know can still
// hold a real key or a button someone held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WireError {
    Short,
    TooLong(usize),
    Kind(u8),
    Slot(u8),
    Flags(u8),
    Buttons,
    // A bit for a make code no key has: 0, or the keyboard's overrun 0xFF.
    HeldKey,
    Count(u8),
    Tag(u8),
    NoKey,
    KeyBits,
    Button,
    Nothing,
    Captured,
    Trailing(usize),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::Short => f.write_str("it ends early"),
            WireError::TooLong(len) => write!(
                f,
                "{len} bytes; an input packet is at most {MAX_INPUT}, {MAX_EVENTS} events"
            ),
            WireError::Kind(kind) => write!(f, "kind {kind} is not one this side takes"),
            WireError::Slot(slot) => write!(f, "slot {slot} is not one this side takes"),
            WireError::Flags(flags) => write!(f, "flags {flags:#04x} are not ones Booth sets"),
            WireError::Buttons => f.write_str("a button held past the five a mouse has"),
            WireError::HeldKey => f.write_str("a key held that no keyboard has"),
            WireError::Count(count) => {
                write!(f, "{count} events; a packet carries at most {MAX_EVENTS}")
            }
            WireError::Tag(tag) => write!(f, "an event of kind {tag}, which Booth never sends"),
            WireError::NoKey => f.write_str("a key event for a make code no key has"),
            WireError::KeyBits => f.write_str("a key event with bits Booth never sets"),
            WireError::Button => {
                f.write_str("a mouse button event past the five buttons, or neither down nor up")
            }
            WireError::Nothing => f.write_str("a move or a wheel turn of nothing"),
            WireError::Captured => f.write_str("a capture time of zero from the controller"),
            WireError::Trailing(len) => write!(f, "{len} bytes after the end"),
        }
    }
}

// What an input packet says besides its events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Head {
    pub slot: u8,
    pub seq: u32,
    // None in a relayed packet the host could not time.
    pub captured: Option<u64>,
    pub about: bool,
    pub held: Held,
}

// A whole packet: the prefix, then everything else. `events` are the
// packet's own, oldest first; the caller keeps them within MAX_EVENTS.
pub(crate) fn write_input(
    kind: u8,
    slot: u8,
    seq: u32,
    captured: u64,
    held: &Held,
    events: &[InputEvent],
    out: &mut Vec<u8>,
) {
    out.push(kind);
    out.push(slot);
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&captured.to_le_bytes());
    out.push(0);
    out.push(held.buttons);
    for word in held.keys {
        out.extend_from_slice(&word.to_le_bytes());
    }
    let events = &events[..events.len().min(MAX_EVENTS)];
    out.push(events.len() as u8);
    for event in events {
        write_event(event, out);
    }
}

fn write_event(event: &InputEvent, out: &mut Vec<u8>) {
    match *event {
        InputEvent::Key { key, down } => {
            out.push(KEY);
            out.push(key.code);
            let mut bits = 0;
            if down {
                bits |= DOWN;
            }
            if key.e0 {
                bits |= E0;
            }
            out.push(bits);
        }
        InputEvent::Move { dx, dy } => {
            out.push(MOVE);
            out.extend_from_slice(&short(dx).to_le_bytes());
            out.extend_from_slice(&short(dy).to_le_bytes());
        }
        InputEvent::At { x, y } => {
            out.push(AT);
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
        }
        InputEvent::Button { button, down } => {
            out.push(BUTTON);
            out.push(button.index());
            out.push(u8::from(down));
        }
        InputEvent::Wheel { delta } => {
            out.push(WHEEL);
            out.extend_from_slice(&short(delta).to_le_bytes());
        }
        InputEvent::HWheel { delta } => {
            out.push(HWHEEL);
            out.extend_from_slice(&short(delta).to_le_bytes());
        }
    }
}

// Callers split anything larger before it gets here (send.rs).
fn short(value: i32) -> i16 {
    value.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

// `kind` is the kind this side takes. The events go into `events`, which is
// cleared first, so a caller that keeps one buffer allocates nothing.
pub(crate) fn read_input(
    payload: &[u8],
    kind: u8,
    events: &mut Vec<InputEvent>,
) -> Result<Head, WireError> {
    super::wipe(events);
    if payload.len() > MAX_INPUT {
        return Err(WireError::TooLong(payload.len()));
    }
    if payload.len() < HEAD {
        return Err(WireError::Short);
    }
    if payload[0] != kind {
        return Err(WireError::Kind(payload[0]));
    }
    let slot = payload[SLOT_AT];
    let allowed = match kind {
        SENT => slot == 0,
        _ => usize::from(slot) < MAX_ROSTER,
    };
    if !allowed {
        return Err(WireError::Slot(slot));
    }
    let seq = u32::from_le_bytes(word(payload, SEQ_AT));
    let captured = u64::from_le_bytes(word(payload, CAPTURED_AT));
    let flags = payload[FLAGS_AT];
    let flags_allowed = if kind == SENT { 0 } else { ABOUT };
    if flags & !flags_allowed != 0 {
        return Err(WireError::Flags(flags));
    }
    if kind == SENT && captured == NOT_KNOWN {
        return Err(WireError::Captured);
    }
    let buttons = payload[BUTTONS_AT];
    if buttons & !BUTTONS != 0 {
        return Err(WireError::Buttons);
    }
    let mut held = Held {
        keys: [0; 8],
        buttons,
    };
    for (i, key) in held.keys.iter_mut().enumerate() {
        *key = u64::from_le_bytes(word(payload, KEYS_AT + 8 * i));
    }
    if Held::NO_KEY.iter().any(|&bit| held.bit(bit)) {
        return Err(WireError::HeldKey);
    }
    let count = payload[COUNT_AT];
    if usize::from(count) > MAX_EVENTS {
        return Err(WireError::Count(count));
    }
    // Nothing of a packet refused part way is kept.
    let mut rest = &payload[HEAD..];
    for _ in 0..count {
        match read_event(rest) {
            Ok((event, after)) => {
                events.push(event);
                rest = after;
            }
            Err(why) => {
                super::wipe(events);
                return Err(why);
            }
        }
    }
    if !rest.is_empty() {
        super::wipe(events);
        return Err(WireError::Trailing(rest.len()));
    }
    Ok(Head {
        slot,
        seq,
        captured: (captured != NOT_KNOWN).then_some(captured),
        about: flags & ABOUT != 0,
        held,
    })
}

fn read_event(bytes: &[u8]) -> Result<(InputEvent, &[u8]), WireError> {
    let (&tag, rest) = bytes.split_first().ok_or(WireError::Short)?;
    let len = match tag {
        KEY | BUTTON => 2,
        MOVE | AT => 4,
        WHEEL | HWHEEL => 2,
        _ => return Err(WireError::Tag(tag)),
    };
    if rest.len() < len {
        return Err(WireError::Short);
    }
    let (body, rest) = rest.split_at(len);
    let pair = |at: usize| [body[at], body[at + 1]];
    let event = match tag {
        KEY => {
            let (code, bits) = (body[0], body[1]);
            if !ScanCode::possible(code) {
                return Err(WireError::NoKey);
            }
            if bits & !(DOWN | E0) != 0 {
                return Err(WireError::KeyBits);
            }
            InputEvent::Key {
                key: ScanCode {
                    code,
                    e0: bits & E0 != 0,
                },
                down: bits & DOWN != 0,
            }
        }
        BUTTON => {
            let button = Button::from_index(body[0]).ok_or(WireError::Button)?;
            let down = match body[1] {
                0 => false,
                1 => true,
                _ => return Err(WireError::Button),
            };
            InputEvent::Button { button, down }
        }
        MOVE => {
            let (dx, dy) = (i16::from_le_bytes(pair(0)), i16::from_le_bytes(pair(2)));
            if dx == 0 && dy == 0 {
                return Err(WireError::Nothing);
            }
            InputEvent::Move {
                dx: i32::from(dx),
                dy: i32::from(dy),
            }
        }
        AT => InputEvent::At {
            x: u16::from_le_bytes(pair(0)),
            y: u16::from_le_bytes(pair(2)),
        },
        _ => {
            let delta = i32::from(i16::from_le_bytes(pair(0)));
            if delta == 0 {
                return Err(WireError::Nothing);
            }
            if tag == WHEEL {
                InputEvent::Wheel { delta }
            } else {
                InputEvent::HWheel { delta }
            }
        }
    };
    Ok((event, rest))
}

fn word<const N: usize>(payload: &[u8], at: usize) -> [u8; N] {
    let mut out = [0; N];
    out.copy_from_slice(&payload[at..at + N]);
    out
}

// The host's copy for the sharer, made in place in a packet read_input took
// as SENT: the controller's slot, whatever it said, and the capture time on
// the host's clock (None when it had no offset for the controller).
pub(crate) fn relay_in_place(payload: &mut [u8], slot: u8, captured: Option<u64>, about: bool) {
    payload[0] = RELAYED;
    payload[SLOT_AT] = slot;
    payload[CAPTURED_AT..CAPTURED_AT + 8]
        .copy_from_slice(&captured.unwrap_or(NOT_KNOWN).to_le_bytes());
    payload[FLAGS_AT] = if about { ABOUT } else { 0 };
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn key(code: u8, e0: bool, down: bool) -> InputEvent {
        InputEvent::Key {
            key: ScanCode { code, e0 },
            down,
        }
    }

    fn held_with(keys: &[ScanCode], buttons: &[Button]) -> Held {
        let mut held = Held::default();
        for &key in keys {
            held.set_key(key, true);
        }
        for &button in buttons {
            held.set_button(button, true);
        }
        held
    }

    fn packet(kind: u8, slot: u8, held: &Held, events: &[InputEvent]) -> Vec<u8> {
        let mut out = Vec::new();
        write_input(
            kind,
            slot,
            41,
            1_700_000_000_123_456,
            held,
            events,
            &mut out,
        );
        out
    }

    fn every_kind() -> Vec<InputEvent> {
        vec![
            key(0x1E, false, true),
            key(0x4F, true, false),
            InputEvent::Move { dx: -3, dy: 250 },
            InputEvent::At { x: 65535, y: 1024 },
            InputEvent::Button {
                button: Button::Forward,
                down: true,
            },
            InputEvent::Wheel { delta: -120 },
            InputEvent::HWheel { delta: 240 },
        ]
    }

    #[test]
    fn every_event_round_trips() {
        let held = held_with(
            &[
                ScanCode {
                    code: 0x1D,
                    e0: false,
                },
                ScanCode {
                    code: 0x1D,
                    e0: true,
                },
                ScanCode {
                    code: 0xFE,
                    e0: true,
                },
            ],
            &[Button::Left, Button::Forward],
        );
        let events = every_kind();
        let bytes = packet(SENT, 0, &held, &events);
        assert_eq!(bytes.len(), HEAD + 3 + 3 + 5 + 5 + 3 + 3 + 3);
        let mut got = Vec::new();
        let head = read_input(&bytes, SENT, &mut got).expect("reads");
        assert_eq!(got, events);
        assert_eq!(head.held, held);
        assert_eq!(
            (head.slot, head.seq, head.captured, head.about),
            (0, 41, Some(1_700_000_000_123_456), false)
        );
        assert!(head.held.key(ScanCode {
            code: 0x1D,
            e0: true
        }));
        assert!(!head.held.key(ScanCode {
            code: 0x1E,
            e0: false
        }));
    }

    // Kind, slot, seq, time, flags, buttons, 64 bytes of keys, count,
    // events.
    #[test]
    fn packet_layout() {
        let held = held_with(
            &[ScanCode {
                code: 0x2A,
                e0: false,
            }],
            &[Button::Right],
        );
        let bytes = packet(SENT, 0, &held, &[key(0x1E, false, true)]);
        assert_eq!(bytes[..6], [1, 0, 41, 0, 0, 0]);
        assert_eq!(bytes[6..14], 1_700_000_000_123_456u64.to_le_bytes());
        assert_eq!(bytes[14..16], [0, 0b10]);
        // Left Shift, 0x2A: word 0, bit 42.
        assert_eq!(bytes[16..24], (1u64 << 42).to_le_bytes());
        assert!(bytes[24..80].iter().all(|&b| b == 0));
        assert_eq!(bytes[80..], [1, KEY, 0x1E, DOWN]);
        assert_eq!(HEAD, 81);
        assert_eq!(MAX_INPUT, 81 + 32 * 5);
    }

    #[test]
    fn host_relays_in_place() {
        let bytes_before = packet(SENT, 0, &Held::default(), &every_kind());
        let mut bytes = bytes_before.clone();
        relay_in_place(&mut bytes, 5, Some(77), true);
        let mut got = Vec::new();
        let head = read_input(&bytes, RELAYED, &mut got).expect("reads relayed");
        assert_eq!((head.slot, head.captured, head.about), (5, Some(77), true));
        assert_eq!(got, every_kind());
        relay_in_place(&mut bytes, 3, None, false);
        let head = read_input(&bytes, RELAYED, &mut got).expect("reads relayed");
        assert_eq!((head.slot, head.captured, head.about), (3, None, false));
        assert_eq!(bytes.len(), bytes_before.len());
        // A relayed packet is not one the host takes from a controller.
        assert_eq!(
            read_input(&bytes, SENT, &mut got),
            Err(WireError::Kind(RELAYED))
        );
    }

    #[test]
    fn bent_packets_refused() {
        let good = packet(SENT, 0, &Held::default(), &[key(0x1E, false, true)]);
        let edit = |at: usize, byte: u8| {
            let mut bad = good.clone();
            bad[at] = byte;
            bad
        };
        let mut trailing = good.clone();
        trailing.push(0);
        let mut too_many = packet(SENT, 0, &Held::default(), &[]);
        too_many[COUNT_AT] = MAX_EVENTS as u8 + 1;
        for _ in 0..=MAX_EVENTS {
            too_many.extend_from_slice(&[WHEEL, 1, 0]);
        }
        let mut no_key = good.clone();
        no_key[KEYS_AT] = 1;
        let mut overrun = good.clone();
        overrun[KEYS_AT + 31] = 0x80;
        let mut overrun_e0 = good.clone();
        overrun_e0[KEYS_AT + 63] = 0x80;
        let mut zero_time = good.clone();
        zero_time[CAPTURED_AT..CAPTURED_AT + 8].fill(0);
        let zero_move = packet(SENT, 0, &Held::default(), &[]);
        let mut zero_move = zero_move;
        zero_move[COUNT_AT] = 1;
        zero_move.extend_from_slice(&[MOVE, 0, 0, 0, 0]);
        let mut zero_wheel = packet(SENT, 0, &Held::default(), &[]);
        zero_wheel[COUNT_AT] = 1;
        zero_wheel.extend_from_slice(&[HWHEEL, 0, 0]);
        let mut bad_button = packet(SENT, 0, &Held::default(), &[]);
        bad_button[COUNT_AT] = 1;
        bad_button.extend_from_slice(&[BUTTON, 5, 1]);
        let mut bad_down = packet(SENT, 0, &Held::default(), &[]);
        bad_down[COUNT_AT] = 1;
        bad_down.extend_from_slice(&[BUTTON, 0, 2]);
        let long = vec![SENT; MAX_INPUT + 1];
        // The first event reads, the second does not.
        let mut second_bad = packet(
            SENT,
            0,
            &Held::default(),
            &[key(0x1E, false, true), key(0x1F, false, true)],
        );
        let second_tag = second_bad.len() - 3;
        second_bad[second_tag] = 9;
        let cases: Vec<(Vec<u8>, WireError)> = vec![
            (second_bad, WireError::Tag(9)),
            (good[..HEAD - 1].to_vec(), WireError::Short),
            (good[..good.len() - 1].to_vec(), WireError::Short),
            (long, WireError::TooLong(MAX_INPUT + 1)),
            (edit(0, 3), WireError::Kind(3)),
            (edit(SLOT_AT, 1), WireError::Slot(1)),
            (edit(FLAGS_AT, ABOUT), WireError::Flags(ABOUT)),
            (edit(BUTTONS_AT, 0x20), WireError::Buttons),
            (no_key, WireError::HeldKey),
            (overrun, WireError::HeldKey),
            (overrun_e0, WireError::HeldKey),
            (too_many, WireError::Count(MAX_EVENTS as u8 + 1)),
            (edit(HEAD, 7), WireError::Tag(7)),
            (edit(HEAD + 1, 0), WireError::NoKey),
            (edit(HEAD + 1, 0xFF), WireError::NoKey),
            (edit(HEAD + 2, 4), WireError::KeyBits),
            (zero_move, WireError::Nothing),
            (zero_wheel, WireError::Nothing),
            (bad_button, WireError::Button),
            (bad_down, WireError::Button),
            (zero_time, WireError::Captured),
            (trailing, WireError::Trailing(1)),
        ];
        let mut events = vec![key(1, false, true)];
        for (bytes, want) in cases {
            assert_eq!(
                read_input(&bytes, SENT, &mut events),
                Err(want),
                "{bytes:?}"
            );
            assert!(events.is_empty(), "nothing of a refused packet is kept");
        }
        // A relayed packet from past the roster.
        let mut far = packet(SENT, 0, &Held::default(), &[]);
        relay_in_place(&mut far, MAX_ROSTER as u8, None, false);
        assert_eq!(
            read_input(&far, RELAYED, &mut events),
            Err(WireError::Slot(MAX_ROSTER as u8))
        );
    }

    #[test]
    fn debug_shows_no_key_and_no_position() {
        let shown = format!(
            "{:?} {:?} {:?}",
            key(0x1E, false, true),
            InputEvent::At { x: 4321, y: 8765 },
            held_with(
                &[ScanCode {
                    code: 0x1E,
                    e0: false
                }],
                &[]
            )
        );
        for secret in ["30", "1e", "1E", "4321", "8765"] {
            assert!(!shown.contains(secret), "{shown}");
        }
    }

    // Why a packet was refused goes to the log. One refused for a single bit
    // this build does not know still carries a real key, a held button or a
    // click, and why names none of them: no number at all.
    #[test]
    fn refusals_name_no_key() {
        let mut new_bit = packet(SENT, 0, &Held::default(), &[key(0x1E, true, true)]);
        new_bit[HEAD + 2] |= 1 << 5;
        let mut held_left = packet(SENT, 0, &Held::default(), &[]);
        held_left[BUTTONS_AT] = 0b10_0001;
        let mut left_half_down = packet(SENT, 0, &Held::default(), &[]);
        left_half_down[COUNT_AT] = 1;
        left_half_down.extend_from_slice(&[BUTTON, 0, 2]);
        let mut events = Vec::new();
        for (bytes, want) in [
            (new_bit, WireError::KeyBits),
            (held_left, WireError::Buttons),
            (left_half_down, WireError::Button),
        ] {
            let why = read_input(&bytes, SENT, &mut events).expect_err("refused");
            assert_eq!(why, want);
            let said = why.to_string();
            assert!(!said.chars().any(|c| c.is_ascii_digit()), "{said}");
        }
    }

    fn any_event() -> impl Strategy<Value = InputEvent> {
        let nonzero = prop_oneof![i16::MIN..0, 1..=i16::MAX].prop_map(i32::from);
        prop_oneof![
            (1u8..=0xFE, any::<bool>(), any::<bool>())
                .prop_map(|(code, e0, down)| key(code, e0, down)),
            (any::<i16>(), 1..=i16::MAX).prop_map(|(dx, dy)| InputEvent::Move {
                dx: i32::from(dx),
                dy: i32::from(dy),
            }),
            (any::<u16>(), any::<u16>()).prop_map(|(x, y)| InputEvent::At { x, y }),
            (0u8..5, any::<bool>()).prop_map(|(index, down)| InputEvent::Button {
                button: Button::from_index(index).expect("a button"),
                down,
            }),
            nonzero
                .clone()
                .prop_map(|delta| InputEvent::Wheel { delta }),
            nonzero.prop_map(|delta| InputEvent::HWheel { delta }),
        ]
    }

    fn any_held() -> impl Strategy<Value = Held> {
        (any::<[u64; 8]>(), 0u8..=BUTTONS).prop_map(|(mut keys, buttons)| {
            for bit in Held::NO_KEY {
                keys[bit / 64] &= !(1 << (bit % 64));
            }
            Held { keys, buttons }
        })
    }

    proptest! {
        #[test]
        fn any_packet_round_trips(
            held in any_held(),
            events in prop::collection::vec(any_event(), 0..=MAX_EVENTS),
            seq in any::<u32>(),
            captured in 1u64..,
            slot in 0u8..MAX_ROSTER as u8,
            about in any::<bool>(),
            relayed_time in prop::option::of(1u64..),
        ) {
            let mut bytes = Vec::new();
            write_input(SENT, 0, seq, captured, &held, &events, &mut bytes);
            let mut got = Vec::new();
            let head = read_input(&bytes, SENT, &mut got).expect("reads");
            prop_assert_eq!(&got, &events);
            prop_assert_eq!(head, Head { slot: 0, seq, captured: Some(captured), about: false, held });
            relay_in_place(&mut bytes, slot, relayed_time, about);
            let head = read_input(&bytes, RELAYED, &mut got).expect("reads relayed");
            prop_assert_eq!(&got, &events);
            prop_assert_eq!(head, Head { slot, seq, captured: relayed_time, about, held });
        }

        // Whatever gets through is a packet a controller following the rules
        // could send: it writes back to the same bytes.
        #[test]
        fn any_bytes_parse_or_are_refused(
            body in prop::collection::vec(any::<u8>(), 0..MAX_INPUT + 8),
            relayed in any::<bool>(),
        ) {
            let kind = if relayed { RELAYED } else { SENT };
            let mut bytes = vec![kind];
            bytes.extend(body);
            let mut events = Vec::new();
            if let Ok(head) = read_input(&bytes, kind, &mut events) {
                let mut again = Vec::new();
                write_input(kind, head.slot, head.seq, head.captured.unwrap_or(0), &head.held, &events, &mut again);
                again[FLAGS_AT] = bytes[FLAGS_AT];
                prop_assert_eq!(again, bytes);
            } else {
                prop_assert!(events.is_empty());
            }
        }
    }

    // Real packets with bytes changed, dropped and added: most get past the
    // first checks, and none panics.
    #[test]
    fn edited_packets_never_panic() {
        let seed = packet(SENT, 0, &held_with(&[], &[Button::Left]), &every_kind());
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut events = Vec::new();
        for _ in 0..50_000 {
            let mut bytes = seed.clone();
            for _ in 0..1 + next() % 4 {
                let at = (next() as usize) % (bytes.len() + 1);
                match next() % 3 {
                    0 if at < bytes.len() => bytes[at] = next() as u8,
                    1 => bytes.insert(at, next() as u8),
                    _ if at < bytes.len() => {
                        bytes.remove(at);
                    }
                    _ => {}
                }
            }
            let _ = read_input(&bytes, SENT, &mut events);
            assert!(events.len() <= MAX_EVENTS);
        }
    }
}
