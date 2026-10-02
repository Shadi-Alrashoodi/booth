// What stands in for the socket and the room between the two threads: the
// viewer's inbox, which the pacer's send function fills as the room's
// receive thread would, and the answers going back to the sharer, both
// through network.rs when the run asks for a network. Nothing here waits:
// the viewer's thread blocks on the inbox and its own timer, and the
// sharer's thread reads what came back before each encode.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use capture::CursorUpdate;
use share::rate::Lost;
use share::{Audience, Back, Inbox, Line, Sent};

pub struct Link {
    pub inbox: Arc<Inbox>,
    back: Mutex<Vec<Back>>,
    // The sharer's parity, for the viewer's log line once a second.
    pub parity: AtomicU32,
    pub knob_dropped: AtomicU64,
    // Whether the viewer decodes HEVC, for a share left to pick its codec:
    // what the probe said before the window opened, then the viewer's own
    // device, then false once it refused a stream of it.
    pub takes_hevc: AtomicBool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Link {
    pub fn new(takes_hevc: bool) -> io::Result<Link> {
        Ok(Link {
            inbox: Arc::new(Inbox::new()?),
            back: Mutex::new(Vec::new()),
            parity: AtomicU32::new(channels::video::PARITY_DEFAULT),
            knob_dropped: AtomicU64::new(0),
            takes_hevc: AtomicBool::new(takes_hevc),
        })
    }

    pub fn send_back(&self, message: Back) {
        lock(&self.back).push(message);
    }

    pub fn stop(&self) {
        self.inbox.stop();
    }

    pub fn stopped(&self) -> bool {
        self.inbox.stopped()
    }
}

// The sharer's end of the link.
pub struct SharerEnd<'a> {
    pub link: &'a Link,
    pub lines: &'a Sender<Line>,
    // For the rate, as the room counts them: frames the viewer reported
    // lost since it last looked, and the frames sent since.
    pub lost: Lost,
    pub sent: Vec<Sent>,
}

impl<'a> SharerEnd<'a> {
    pub fn new(link: &'a Link, lines: &'a Sender<Line>) -> SharerEnd<'a> {
        SharerEnd {
            link,
            lines,
            lost: Lost::default(),
            sent: Vec::new(),
        }
    }
}

impl Audience for SharerEnd<'_> {
    fn cursor(&mut self, update: CursorUpdate) {
        self.link.inbox.cursor(update);
    }

    fn back(&mut self, into: &mut Vec<Back>) {
        for message in lock(&self.link.back).drain(..) {
            self.lost.heard(&message);
            into.push(message);
        }
    }

    fn takes_hevc(&mut self) -> bool {
        self.link.takes_hevc.load(Ordering::Relaxed)
    }

    fn sent(&mut self, frame: &Sent) {
        self.link.inbox.encode_ms(frame.encode_ms);
        self.link.parity.store(frame.parity, Ordering::Relaxed);
        self.lost.sent(frame.number);
        self.sent.push(*frame);
    }

    fn line(&mut self, line: Line) {
        let _ = self.lines.send(line);
    }
}

// Each thread holds one, so a thread that ends in any way, a panic included,
// stops the other. Otherwise the sharer would go on capturing with no
// window, and main would wait forever for its lines.
pub struct StopOnDrop(pub Arc<Link>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thread_that_panics_stops_the_link() {
        let link = Arc::new(Link::new(true).unwrap());
        let thread = std::thread::spawn({
            let link = Arc::clone(&link);
            move || {
                let _stop = StopOnDrop(link);
                panic!("on purpose, to show the other side stops");
            }
        });
        assert!(thread.join().is_err());
        assert!(link.stopped());
    }

    #[test]
    fn answers_reach_the_sharer_once_in_order() {
        let link = Link::new(true).unwrap();
        let (lines, _said) = std::sync::mpsc::channel();
        let mut end = SharerEnd::new(&link, &lines);
        link.send_back(Back::Recover { first: 3, last: 4 });
        link.send_back(Back::Loss(Some(1.5)));
        link.send_back(Back::Idr { seen: 9 });
        let mut back = Vec::new();
        end.back(&mut back);
        assert_eq!(
            back,
            [
                Back::Recover { first: 3, last: 4 },
                Back::Loss(Some(1.5)),
                Back::Idr { seen: 9 }
            ]
        );
        assert_eq!(end.lost.take(), 3, "as the room counts them for the rate");
        end.back(&mut back);
        assert_eq!(back.len(), 3);
    }
}
