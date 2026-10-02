// What reaches the viewer's thread from the caller's: video packets and
// pointer updates, as the room's receive thread or the loopback's link hands
// them over, and what the caller knows that the strip shows. Nothing here
// waits: the viewer's thread blocks on the wake event and its own timer,
// and takes everything each time it wakes.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use capture::CursorUpdate;
use net::pace::Signal;

use crate::Clock;
use crate::cursor;
use crate::screen::LinkNumbers;

// Packets waiting for the viewer's thread, as a socket's receive buffer
// would hold them: about 4.8 MB of 1162-byte packets, 5.5 MB of 1354-byte
// ones on the LAN, half a second of packets at 80 Mbit/s. An 80 Mbit/s
// share with the most parity sends half as much again on top, so for it
// this is about a third of a second. Past it packets are dropped and
// counted, as a full socket buffer drops them.
const MAX_WAITING: usize = 4096;
const MAX_SPARE: usize = 256;

// The sharer's encode times between two strip updates, a quarter second
// apart: 30 at 120 fps. More only piles up while nobody takes them.
const MOST_ENCODE_TIMES: usize = 256;

#[derive(Default)]
struct Waiting {
    packets: VecDeque<Vec<u8>>,
    spare: Vec<Vec<u8>>,
    cursor: Option<CursorUpdate>,
    encode_ms: Vec<f32>,
    link: Option<LinkNumbers>,
    fps: Option<u32>,
    clock: Option<Clock>,
    control: Option<Option<Control>>,
}

// This PC controls the share shown, as the room knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Control {
    // The shared PC has an administrator window in front, which drops
    // everything sent to it: the strip says control is paused.
    pub paused: bool,
}

pub struct Inbox {
    waiting: Mutex<Waiting>,
    wake: Signal,
    stop: AtomicBool,
    close: AtomicBool,
    overflow: AtomicU64,
}

// What the viewer's thread took at one wake besides the packets.
pub(crate) struct Taken {
    pub cursor: Option<CursorUpdate>,
    pub link: Option<LinkNumbers>,
    pub fps: Option<u32>,
    pub clock: Option<Clock>,
    // Some when control changed since the last take.
    pub control: Option<Option<Control>>,
}

impl Inbox {
    pub fn new() -> io::Result<Inbox> {
        Ok(Inbox {
            waiting: Mutex::new(Waiting::default()),
            wake: Signal::new()?,
            stop: AtomicBool::new(false),
            close: AtomicBool::new(false),
            overflow: AtomicU64::new(0),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Waiting> {
        self.waiting.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // One video packet, the channel's payload after the room's prefix.
    pub fn packet(&self, packet: &[u8]) {
        let mut waiting = self.lock();
        if waiting.packets.len() >= MAX_WAITING {
            self.overflow.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let mut buffer = waiting.spare.pop().unwrap_or_default();
        buffer.extend_from_slice(packet);
        waiting.packets.push_back(buffer);
        // The viewer takes everything each time it wakes, so one wake per
        // batch is enough.
        let first = waiting.packets.len() == 1;
        drop(waiting);
        if first {
            self.wake.set();
        }
    }

    // A pointer update never waits behind video. One the viewer has not
    // taken yet is replaced, but its shape is kept if the new one has none,
    // since a shape is only sent when it changes.
    pub fn cursor(&self, update: CursorUpdate) {
        let mut waiting = self.lock();
        let update = cursor::newer(waiting.cursor.take(), update);
        waiting.cursor = Some(update);
        drop(waiting);
        self.wake.set();
    }

    // The sharer's encode time for a frame, for the strip's "enc".
    pub fn encode_ms(&self, ms: f32) {
        let mut waiting = self.lock();
        if waiting.encode_ms.len() < MOST_ENCODE_TIMES {
            waiting.encode_ms.push(ms);
        }
    }

    // The strip's link numbers from now on; they show at its next update.
    pub fn set_link(&self, link: LinkNumbers) {
        self.lock().link = Some(link);
    }

    // The sharer's frame rate changed, as when the room steps a share down
    // to 60 fps: frames wait one interval of the new rate.
    pub fn set_fps(&self, fps: u32) {
        self.lock().fps = Some(fps);
    }

    // A better reading of the sharer's clock, for capture to display.
    pub fn set_clock(&self, clock: Clock) {
        self.lock().clock = Some(clock);
    }

    // This PC controls the share shown from now on, or no longer. The
    // viewer's thread is woken for it: an end stops the capture, and it
    // must not wait for the next frame of a still screen.
    pub fn set_control(&self, control: Option<Control>) {
        self.lock().control = Some(control);
        self.wake.set();
    }

    // Ends the viewer's loop at its next wake, which this causes.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.wake.set();
    }

    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    // Ends the viewer's loop as the person closing its window does, which
    // is how the loop's caller tells the two apart from stop(): stopped()
    // stays false. For tests, which never click on a window.
    pub fn close(&self) {
        self.close.store(true, Ordering::Release);
        self.wake.set();
    }

    pub fn closed(&self) -> bool {
        self.close.load(Ordering::Acquire)
    }

    // Packets dropped because the viewer's thread fell half a second behind.
    pub fn overflow(&self) -> u64 {
        self.overflow.load(Ordering::Relaxed)
    }

    // Wakes the viewer's thread without giving it anything, for the
    // viewer window's own events (viewer::Options::wake).
    pub fn wake(&self) {
        self.wake.set();
    }

    pub(crate) fn signal(&self) -> &Signal {
        &self.wake
    }

    // Swaps the waiting packets into `batch`. The buffers of the batch
    // before go back to be filled again.
    pub(crate) fn take(&self, batch: &mut VecDeque<Vec<u8>>) -> Taken {
        let mut waiting = self.lock();
        for mut buffer in batch.drain(..) {
            if waiting.spare.len() < MAX_SPARE {
                buffer.clear();
                waiting.spare.push(buffer);
            }
        }
        std::mem::swap(&mut waiting.packets, batch);
        Taken {
            cursor: waiting.cursor.take(),
            link: waiting.link.take(),
            fps: waiting.fps.take(),
            clock: waiting.clock.take(),
            control: waiting.control.take(),
        }
    }

    pub(crate) fn take_encode_ms(&self) -> Vec<f32> {
        std::mem::take(&mut self.lock().encode_ms)
    }
}

#[cfg(test)]
mod tests {
    use capture::{CursorKind, CursorShape};

    use super::*;

    fn at(x: i32, shape: Option<CursorShape>) -> CursorUpdate {
        CursorUpdate {
            x,
            y: 0,
            visible: true,
            scale: 1.0,
            shape,
        }
    }

    fn arrow() -> CursorShape {
        CursorShape {
            kind: CursorKind::Color,
            width: 1,
            height: 1,
            pitch: 4,
            hotspot_x: 0,
            hotspot_y: 0,
            bytes: vec![255; 4],
        }
    }

    #[test]
    fn a_newer_position_keeps_a_shape_not_taken_yet() {
        let inbox = Inbox::new().unwrap();
        inbox.cursor(at(1, Some(arrow())));
        inbox.cursor(at(2, None));
        let taken = inbox.take(&mut VecDeque::new()).cursor.unwrap();
        assert_eq!((taken.x, taken.shape), (2, Some(arrow())));
        assert!(inbox.take(&mut VecDeque::new()).cursor.is_none());
    }

    #[test]
    fn packets_in_order_and_overflow_counted() {
        let inbox = Inbox::new().unwrap();
        for n in 0..MAX_WAITING + 3 {
            inbox.packet(&(n as u32).to_le_bytes());
        }
        let mut batch = VecDeque::new();
        inbox.take(&mut batch);
        assert_eq!(batch.len(), MAX_WAITING);
        assert!(
            batch
                .iter()
                .enumerate()
                .all(|(n, packet)| packet[..] == (n as u32).to_le_bytes())
        );
        assert_eq!(inbox.overflow(), 3);
        // The next take hands the buffers back for reuse.
        inbox.packet(&[7]);
        inbox.take(&mut batch);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0], [7]);
    }

    #[test]
    fn what_the_caller_sets_is_taken_once() {
        let inbox = Inbox::new().unwrap();
        inbox.set_fps(60);
        inbox.set_link(LinkNumbers::default());
        let taken = inbox.take(&mut VecDeque::new());
        assert_eq!(taken.fps, Some(60));
        assert!(taken.link.is_some());
        let again = inbox.take(&mut VecDeque::new());
        assert!(again.fps.is_none() && again.link.is_none() && again.clock.is_none());
        // Control's start and end are each taken once, the newest winning.
        inbox.set_control(Some(Control { paused: false }));
        inbox.set_control(Some(Control { paused: true }));
        assert_eq!(
            inbox.take(&mut VecDeque::new()).control,
            Some(Some(Control { paused: true }))
        );
        inbox.set_control(None);
        assert_eq!(inbox.take(&mut VecDeque::new()).control, Some(None));
        assert_eq!(inbox.take(&mut VecDeque::new()).control, None);
        for n in 0..MOST_ENCODE_TIMES + 5 {
            inbox.encode_ms(n as f32);
        }
        assert_eq!(inbox.take_encode_ms().len(), MOST_ENCODE_TIMES);
        assert!(inbox.take_encode_ms().is_empty());
        assert!(!inbox.stopped());
        inbox.stop();
        assert!(inbox.stopped());
    }
}
