//! The video path between a screen and a viewer window, whatever carries the
//! packets: the loopback's in-process link or the room's socket.
//!
//! The sharer's side ([`Sharer`]) takes the screen or the test pattern,
//! encodes each frame, cuts it into packets with parity and hands them to
//! the pacer, whose send function the caller gives. The viewer's side
//! ([`Screen`]) takes packets and pointer updates from an [`Inbox`] the
//! caller fills, puts frames back together, decodes and presents them, and
//! reports what goes back to the sharer. Each runs on a thread of the
//! caller's.

mod cursor;
mod inbox;
mod knob;
mod numbers;
pub mod rate;
mod recovery;
mod resolution;
mod screen;
mod sharer;
mod source;

use std::time::{Duration, Instant};

pub use inbox::{Control, Inbox};
pub use knob::{Knob, fresh_seed};
pub use numbers::{
    SharerNumbers, ViewerNumbers, end_to_end_level, ms, path_word, spread, spread_text, stage_level,
};
pub use recovery::Back;
pub use resolution::{FineTimer, fine_timers_held};
pub use screen::{LinkNumbers, Report, Screen, Second, Watch, Window, primary_takes_hevc};
pub use sharer::{
    Audience, SOFTWARE_DID_NOT_START, SOFTWARE_FAILED_TOO, SWITCH_GAP, Sent, Setup, Sharer,
    Software,
};
pub use source::Choice;

// The other crates' types this crate's own take and give, so a caller such
// as the room needs no dependency on capture, encode or viewer for them.
pub use capture::{
    Adapter, CursorKind, CursorShape, CursorUpdate, Monitor, MonitorId, PauseReason, adapters,
    make_process_dpi_aware, monitors,
};
pub use encode::{Codec, Kind, Preset, Settings};
pub use viewer::{
    Capturing, ControlOut, LinkState, MouseButton, MouseMode, PathWord, Pointing, PresentPath, Show,
};

// A datagram of 1200 bytes less the session's 32, the channel byte and the
// room's two-byte prefix. It fits the 1280-byte tunnel MTU of Tailscale and
// IPv6 with room to spare.
pub const PAYLOAD_INTERNET: usize = 1200 - 32 - 1 - 2;
// The same from a 1400-byte datagram, when every viewer is on the LAN.
pub const PAYLOAD_LAN: usize = 1400 - 32 - 1 - 2;

// What a share has to say, worded as a log line: some name the GPU or quote
// FFmpeg. Say is for whoever runs it: stderr in the loopback. Log goes to
// booth.log only. The room logs both and never shows either in chat; what
// the person needs to know it says in its own words, from what the Sharer
// and the Screen report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    Say(String),
    Log(String),
}

// The clock a frame's capture and encode times are written in, as
// microseconds: what it read at `epoch`, and one microsecond more for every
// one since. The loopback's reads 0 when it starts; the room's is the ping
// clock, and a viewer converts the sharer's with the clock offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clock {
    pub epoch: Instant,
    pub at_epoch: u64,
}

impl Clock {
    pub fn starting(epoch: Instant) -> Clock {
        Clock { epoch, at_epoch: 0 }
    }

    pub fn micros(&self, at: Instant) -> u64 {
        let since = at.saturating_duration_since(self.epoch).as_micros() as u64;
        self.at_epoch.saturating_add(since)
    }

    // The moment the clock read `micros`, if this PC's clock reaches back
    // that far.
    pub fn instant(&self, micros: u64) -> Option<Instant> {
        if micros >= self.at_epoch {
            self.epoch
                .checked_add(Duration::from_micros(micros - self.at_epoch))
        } else {
            self.epoch
                .checked_sub(Duration::from_micros(self.at_epoch - micros))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clock_reads_back_the_moments_it_wrote() {
        let epoch = Instant::now();
        let later = epoch + Duration::from_micros(1500);
        let starting = Clock::starting(epoch);
        assert_eq!(starting.micros(later), 1500);
        assert_eq!(starting.instant(1500), Some(later));
        // A room's clock reads the wall clock's microseconds at its epoch.
        let room = Clock {
            epoch: later,
            at_epoch: 1_000_000,
        };
        assert_eq!(room.micros(later + Duration::from_micros(7)), 1_000_007);
        assert_eq!(room.instant(1_000_000 - 1500), Some(epoch));
        assert_eq!(room.micros(epoch), 1_000_000, "never before its epoch");
    }
}
