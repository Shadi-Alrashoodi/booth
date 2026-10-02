// An encoded frame split into packets with Reed-Solomon parity on the sharer,
// and put back together on the viewer, in order, with a deadline of one frame
// interval and no queue. The room puts its own prefix in front of each packet
// and wires the recover requests and the loss number through; the send thread
// that paces the packets is net::pace.

mod loss;
mod packetize;
mod reassemble;
mod wire;

pub use loss::{
    LOSS_WINDOW, PARITY_CEILING, PARITY_DEFAULT, PARITY_FLOOR, VideoLoss, lost_for_good,
    parity_for_loss, parity_percent,
};
pub use packetize::{PacketizeError, Packetizer, Packets, parity_count};
pub use reassemble::{
    Arrival, DropReason, Event, Frame, MAX_HELD_BYTES, MAX_PENDING, RECOVER_GAP, Reassembler,
    SHORTEST_WAIT, VideoNumbers,
};
pub use wire::{
    FRAME_HEADER, FrameFacts, HEADER, MAX_DATA, MAX_SHARD, MIN_SHARD, Packet, PacketError,
    SHARD_STEP, read_packet,
};
