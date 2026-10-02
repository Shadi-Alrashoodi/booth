use std::time::Instant;

use channels::{Channel, FrameError, PingMessage, Reliable, frame, unframe};
use proptest::prelude::*;

const ALL: [Channel; 7] = [
    Channel::Control,
    Channel::Chat,
    Channel::Voice,
    Channel::Video,
    Channel::Cursor,
    Channel::Input,
    Channel::Ping,
];

#[test]
fn every_channel_round_trips() {
    for (byte, channel) in ALL.into_iter().enumerate() {
        let mut out = Vec::new();
        frame(channel, b"payload", &mut out);
        assert_eq!(out[0] as usize, byte);
        assert_eq!(unframe(&out), Ok((channel, &b"payload"[..])));
    }
}

#[test]
fn empty_payload_is_allowed() {
    let mut out = Vec::new();
    frame(Channel::Control, &[], &mut out);
    assert_eq!(out, [0]);
    assert_eq!(unframe(&out), Ok((Channel::Control, &[][..])));
}

#[test]
fn frame_appends() {
    let mut out = vec![9, 9];
    frame(Channel::Chat, b"hi", &mut out);
    assert_eq!(out, [9, 9, 1, b'h', b'i']);
}

#[test]
fn empty_packet_is_an_error() {
    assert_eq!(unframe(&[]), Err(FrameError::Empty));
}

#[test]
fn unknown_channel_is_an_error() {
    for byte in 7..=255u8 {
        assert_eq!(
            unframe(&[byte, 1, 2, 3]),
            Err(FrameError::UnknownChannel(byte))
        );
    }
}

#[test]
fn ping_layout_is_a_kind_byte_then_little_endian_fields() {
    let mut out = Vec::new();
    PingMessage::Ping {
        seq: 0x0403_0201,
        t1: 0x0c0b_0a09_0807_0605,
    }
    .encode(&mut out);
    assert_eq!(out, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);

    out.clear();
    PingMessage::Pong {
        seq: 1,
        t1: 2,
        t2: 3,
        t3: u64::MAX,
    }
    .encode(&mut out);
    let mut expected = vec![1, 1, 0, 0, 0];
    expected.extend_from_slice(&2u64.to_le_bytes());
    expected.extend_from_slice(&3u64.to_le_bytes());
    expected.extend_from_slice(&[0xff; 8]);
    assert_eq!(out, expected);
}

#[test]
fn ping_decode_rejects_wrong_lengths_and_kinds() {
    let mut ping = Vec::new();
    PingMessage::Ping { seq: 5, t1: 6 }.encode(&mut ping);
    let mut pong = Vec::new();
    PingMessage::Pong {
        seq: 5,
        t1: 6,
        t2: 7,
        t3: 8,
    }
    .encode(&mut pong);

    assert_eq!(PingMessage::decode(&[]), Err(FrameError::Empty));
    assert_eq!(PingMessage::decode(&[2]), Err(FrameError::UnknownKind(2)));
    assert_eq!(
        PingMessage::decode(&ping[..12]),
        Err(FrameError::Length {
            expected: 13,
            actual: 12
        })
    );
    ping.push(0);
    assert_eq!(
        PingMessage::decode(&ping),
        Err(FrameError::Length {
            expected: 13,
            actual: 14
        })
    );
    // A pong cut to a ping's length is still a malformed pong.
    assert_eq!(
        PingMessage::decode(&pong[..13]),
        Err(FrameError::Length {
            expected: 29,
            actual: 13
        })
    );
}

#[test]
fn errors_read_plainly() {
    assert_eq!(FrameError::Empty.to_string(), "empty packet");
    assert_eq!(
        FrameError::UnknownChannel(9).to_string(),
        "unknown channel 9"
    );
    assert_eq!(
        FrameError::Length {
            expected: 13,
            actual: 4
        }
        .to_string(),
        "message is 4 bytes, expected 13"
    );
}

fn any_ping() -> impl Strategy<Value = PingMessage> {
    prop_oneof![
        (any::<u32>(), any::<u64>()).prop_map(|(seq, t1)| PingMessage::Ping { seq, t1 }),
        (any::<u32>(), any::<u64>(), any::<u64>(), any::<u64>())
            .prop_map(|(seq, t1, t2, t3)| PingMessage::Pong { seq, t1, t2, t3 }),
    ]
}

proptest! {
    #[test]
    fn any_payload_round_trips(
        index in 0..ALL.len(),
        payload in proptest::collection::vec(any::<u8>(), 0..1500),
    ) {
        let channel = ALL[index];
        let mut out = Vec::new();
        frame(channel, &payload, &mut out);
        prop_assert_eq!(unframe(&out), Ok((channel, &payload[..])));
    }

    #[test]
    fn any_ping_round_trips(message in any_ping()) {
        let mut out = Vec::new();
        message.encode(&mut out);
        prop_assert_eq!(PingMessage::decode(&out), Ok(message));
    }

    #[test]
    fn random_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..1300)) {
        let _ = unframe(&bytes);
        let _ = PingMessage::decode(&bytes);

        let now = Instant::now();
        let mut fresh = Reliable::new();
        let _ = fresh.receive(&bytes, now);
        while fresh.poll_transmit(now, None).is_some() {}
        while fresh.next_delivered().is_some() {}

        let mut busy = Reliable::new();
        for i in 0..70u8 {
            busy.send(&[i]).unwrap();
        }
        while busy.poll_transmit(now, None).is_some() {}
        let _ = busy.receive(&bytes, now);
        prop_assert!(busy.counters().in_flight <= channels::reliable::WINDOW);
    }
}
