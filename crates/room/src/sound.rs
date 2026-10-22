// PC sound in a room: what a sharer's PC plays, sent with the share on a
// stream of its own (Channel::Sound) to the share's watchers whose copies
// read it (peer::PC_SOUND_SINCE), through the host as video is.

mod wire;

pub(crate) use wire::{MAX_SOUND, read_sent, relay_in_place};

#[cfg(test)]
pub(crate) use wire::SoundFrame;

// A sharer sends 100 frames a second, 10 ms each. Past this rate the host
// drops what comes, since each frame goes on to every watcher; the burst
// covers a sharer's capture thread catching up after a stall.
pub(crate) const SOUND_PER_SECOND: f64 = 150.0;
pub(crate) const SOUND_BURST: f64 = 50.0;
