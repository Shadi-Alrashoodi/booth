use super::{MAX_MESSAGE, ReliableError};
use crate::reader::Reader;

const KIND_DATA: u8 = 0;
const KIND_ACK: u8 = 1;
const ACK_LEN: usize = 1 + 4 + 8;
pub(super) const DATA_HEADER: usize = ACK_LEN + 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Ack {
    // The next sequence the receiver is waiting for; everything before it
    // has been received.
    pub(super) next: u32,
    // Bit i set means sequence next + 1 + i has been received. A receiver
    // holds at most WINDOW - 1 messages past next, so 64 bits report every
    // one of them and a sender never resends a message that already arrived.
    pub(super) bits: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Frame<'a> {
    Data {
        ack: Ack,
        seq: u32,
        message: &'a [u8],
    },
    Ack(Ack),
}

impl Frame<'_> {
    pub(super) fn ack(&self) -> Ack {
        match *self {
            Frame::Data { ack, .. } | Frame::Ack(ack) => ack,
        }
    }
}

pub(super) fn parse(buf: &[u8]) -> Result<Frame<'_>, ReliableError> {
    let wrong_length = ReliableError::Length(buf.len());
    let (&kind, body) = buf.split_first().ok_or(wrong_length)?;
    if kind != KIND_DATA && kind != KIND_ACK {
        return Err(ReliableError::UnknownKind(kind));
    }

    let mut body = Reader::new(body);
    let ack = Ack {
        next: body.u32().ok_or(wrong_length)?,
        bits: body.u64().ok_or(wrong_length)?,
    };
    if kind == KIND_ACK {
        return if body.rest().is_empty() {
            Ok(Frame::Ack(ack))
        } else {
            Err(wrong_length)
        };
    }

    let seq = body.u32().ok_or(wrong_length)?;
    let message = body.rest();
    if message.len() > MAX_MESSAGE {
        return Err(ReliableError::TooBig(message.len()));
    }
    Ok(Frame::Data { ack, seq, message })
}

pub(super) fn data(ack: Ack, seq: u32, message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(DATA_HEADER + message.len());
    out.push(KIND_DATA);
    put_ack(&mut out, ack);
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(message);
    out
}

pub(super) fn ack_only(ack: Ack) -> Vec<u8> {
    let mut out = Vec::with_capacity(ACK_LEN);
    out.push(KIND_ACK);
    put_ack(&mut out, ack);
    out
}

fn put_ack(out: &mut Vec<u8>, ack: Ack) {
    out.extend_from_slice(&ack.next.to_le_bytes());
    out.extend_from_slice(&ack.bits.to_le_bytes());
}
