// The send thread on the sharer. When a viewer is on an internet path, one
// frame's packets go out spread over at most half a frame interval, so a
// voice or input packet sent meanwhile is never stuck behind a whole frame in
// the router's queue. When every viewer is on the LAN they go out at once.
// The thread holds one frame at most: a newer frame sends the rest of the
// older one at once and starts, so nothing waits behind a newer frame and
// nothing queues. A frame the thread has not started when a newer one comes
// is dropped, not sent late (put below).
//
// The thread blocks on the mailbox's event and the high-resolution timer
// together. A sleep or a timed wait would run on the default 15.6 ms timer,
// which is longer than a whole 120 fps frame.
//
// Each wake sends every packet whose time on the even schedule has come.
// The timer wakes about 0.5 ms late whatever the wait (tests/pace.rs), so the
// packets leave in clumps, one per wake: at 120 fps, 72 packets are due 30
// microseconds apart and go as the first alone, then about 17 at a time
// every half millisecond. Waking per packet cannot do better on this timer,
// and on a finer one the clumps get smaller by themselves.

mod timer;

use std::any::Any;
use std::fmt;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows_sys::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};

pub use timer::{Signal, Timer, Woken, wait};

// The high-resolution timer wakes about 0.5 ms late, under 1 ms late at the
// 99th percentile, and once in a few thousand wakes 1.5 to 3 ms late on a
// busy PC (tests/pace.rs). The spread ends this much before half the
// interval, so the last packet leaves inside it all but that rarely. With
// 1 ms it once left 4.13 ms after the first at 120 fps, 36 microseconds short
// of the limit, and with 1.5 ms 4.00 ms. More would leave a 120 fps spread
// so short that one wake that late sends nearly the whole frame at once. No
// margin covers a stall of the whole PC, which once held a wait up 7.7 ms.
pub const SPREAD_MARGIN: Duration = Duration::from_millis(2);

// A frame interval longer than this is not a video frame rate, and spreading
// over half of it would hold packets back for no reason.
const LONGEST_SPREAD: Duration = Duration::from_millis(50);

// Packets for one frame, each kept whole, in one buffer that is reused.
#[derive(Default)]
pub struct Burst {
    bytes: Vec<u8>,
    ends: Vec<usize>,
}

impl Burst {
    pub fn new() -> Burst {
        Burst::default()
    }

    pub fn push(&mut self, packet: &[u8]) {
        self.bytes.extend_from_slice(packet);
        self.ends.push(self.bytes.len());
    }

    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    pub fn clear(&mut self) {
        self.bytes.clear();
        self.ends.clear();
    }

    pub fn packet(&self, index: usize) -> Option<&[u8]> {
        let end = *self.ends.get(index)?;
        let start = match index {
            0 => 0,
            _ => self.ends[index - 1],
        };
        Some(&self.bytes[start..end])
    }
}

impl fmt::Debug for Burst {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Burst")
            .field("packets", &self.len())
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PaceNumbers {
    // Frames the thread took from the mailbox.
    pub frames: u64,
    pub packets: u64,
    // Frames whose last packets went at once because a newer frame came
    // while they were being spread.
    pub cut_short: u64,
    // Frames never sent: the thread had not taken them when a newer one
    // came. Only a thread held up for a whole frame interval, in the send
    // function or waiting for a core, leaves one there that long.
    pub discarded: u64,
}

struct Job {
    burst: Burst,
    interval: Duration,
    spread: bool,
}

#[derive(Default)]
struct Mailbox {
    job: Option<Job>,
    spare: Vec<Burst>,
    stop: bool,
    failed: Option<String>,
}

struct Shared {
    mailbox: Mutex<Mailbox>,
    signal: Signal,
    frames: AtomicU64,
    packets: AtomicU64,
    cut_short: AtomicU64,
    discarded: AtomicU64,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Mailbox> {
        self.mailbox.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub struct Pacer {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    note: Option<String>,
}

impl Pacer {
    // `send` runs on the send thread for every packet, in order. With a timer
    // that is not high resolution (Windows before 10 1803) every frame goes
    // at once, spread or not, since that timer cannot wake inside half a
    // frame interval; note() then says so.
    pub fn start<F>(send: F) -> io::Result<Pacer>
    where
        F: FnMut(&[u8]) + Send + 'static,
    {
        let timer = Timer::new()?;
        Pacer::start_with(timer, send)
    }

    pub(crate) fn start_with<F>(timer: Timer, send: F) -> io::Result<Pacer>
    where
        F: FnMut(&[u8]) + Send + 'static,
    {
        let note = timer
            .note()
            .map(|note| format!("{note}; video packets go out without spreading"));
        let shared = Arc::new(Shared {
            mailbox: Mutex::new(Mailbox::default()),
            signal: Signal::new()?,
            frames: AtomicU64::new(0),
            packets: AtomicU64::new(0),
            cut_short: AtomicU64::new(0),
            discarded: AtomicU64::new(0),
        });
        let thread = thread::Builder::new()
            .name("video send".into())
            .spawn({
                let shared = Arc::clone(&shared);
                move || {
                    // A send function that panics ends the thread. Without
                    // this, put() would go on filling the mailbox for a
                    // thread that is gone, and failure() would say nothing.
                    // Nothing the closure touched is used after the panic.
                    let ran = panic::catch_unwind(AssertUnwindSafe(|| run(&shared, &timer, send)));
                    if let Err(payload) = ran {
                        stop_failed(
                            &shared,
                            format!("it panicked: {}", panic_message(&*payload)),
                        );
                    }
                }
            })
            .map_err(|err| {
                io::Error::new(
                    err.kind(),
                    format!("could not start the video send thread: {err}"),
                )
            })?;
        Ok(Pacer {
            shared,
            thread: Some(thread),
            note,
        })
    }

    // The line to log once when the timer is the fallback.
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    // An empty burst to fill, with the room a sent one left behind.
    pub fn burst(&self) -> Burst {
        let mut burst = self.shared.lock().spare.pop().unwrap_or_default();
        burst.clear();
        burst
    }

    // Hands one frame to the thread. `interval` is the sharer's frame
    // interval; with `spread` the packets leave evenly over half of it, less
    // SPREAD_MARGIN, otherwise all at once.
    //
    // A frame still in the mailbox is dropped for this one. The thread takes
    // a frame within microseconds unless it is held up, by a send that
    // blocks on a full socket buffer or by a busy PC. Then that frame is a
    // frame interval late already, and sending it first would put this one
    // behind it and pile onto whatever held the thread up. The viewer asks
    // for recovery as for any frame lost on the way.
    pub fn put(&self, burst: Burst, interval: Duration, spread: bool) {
        let job = Job {
            burst,
            interval,
            spread,
        };
        let mut mailbox = self.shared.lock();
        if mailbox.failed.is_some() {
            keep_spare(&mut mailbox, job.burst);
            return;
        }
        if let Some(old) = mailbox.job.replace(job) {
            keep_spare(&mut mailbox, old.burst);
            self.shared.discarded.fetch_add(1, Ordering::Relaxed);
        }
        drop(mailbox);
        self.shared.signal.set();
    }

    pub fn numbers(&self) -> PaceNumbers {
        PaceNumbers {
            frames: self.shared.frames.load(Ordering::Relaxed),
            packets: self.shared.packets.load(Ordering::Relaxed),
            cut_short: self.shared.cut_short.load(Ordering::Relaxed),
            discarded: self.shared.discarded.load(Ordering::Relaxed),
        }
    }

    // Why the thread stopped on its own, if it did: a broken event or timer
    // handle, or a send function that panicked. Frames are no longer sent.
    pub fn failure(&self) -> Option<String> {
        self.shared.lock().failed.clone()
    }
}

impl Drop for Pacer {
    // Packets not yet sent are dropped. The thread wakes on the event within
    // microseconds, so this returns about as fast as a thread can be joined.
    fn drop(&mut self) {
        self.shared.lock().stop = true;
        self.shared.signal.set();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl fmt::Debug for Pacer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pacer")
            .field("numbers", &self.numbers())
            .finish()
    }
}

// The frame being sent: packet `next` is the next to go.
struct Going {
    job: Job,
    next: usize,
    start: Instant,
    span: Duration,
}

impl Going {
    fn new(job: Job, start: Instant, spreads: bool) -> Going {
        let span = if spreads && job.spread {
            (job.interval.min(LONGEST_SPREAD) / 2).saturating_sub(SPREAD_MARGIN)
        } else {
            Duration::ZERO
        };
        Going {
            job,
            next: 0,
            start,
            span,
        }
    }

    // Packet `index` is due this long after the start: evenly over the span,
    // the first at zero.
    fn due(&self, index: usize) -> Duration {
        match self.job.burst.len() {
            0 | 1 => Duration::ZERO,
            count => self.span * index as u32 / count as u32,
        }
    }

    fn send_due(&mut self, now: Instant, send: &mut impl FnMut(&[u8]), shared: &Shared) {
        let elapsed = now.saturating_duration_since(self.start);
        while self.next < self.job.burst.len() && self.due(self.next) <= elapsed {
            self.send_one(send, shared);
        }
    }

    fn send_rest(&mut self, send: &mut impl FnMut(&[u8]), shared: &Shared) {
        while self.next < self.job.burst.len() {
            self.send_one(send, shared);
        }
    }

    fn send_one(&mut self, send: &mut impl FnMut(&[u8]), shared: &Shared) {
        if let Some(packet) = self.job.burst.packet(self.next) {
            send(packet);
            shared.packets.fetch_add(1, Ordering::Relaxed);
        }
        self.next += 1;
    }

    fn done(&self) -> bool {
        self.next >= self.job.burst.len()
    }

    fn next_due(&self) -> Instant {
        self.start + self.due(self.next)
    }
}

// Raises the calling thread, for the rest of its life, when it wakes on a
// timer and must act at once: the send thread here, and the room's timer
// thread. A game can keep every core busy, and a thread at normal priority
// then waits for a time slice after its timer fires: the spread stretches,
// and the room's timer thread woke 6 to 24 ms late at the 99th percentile.
// Neither spins; each blocks between wakes. A pass of the room's timer
// thread took 0.02 to 0.1 ms at the median and 0.9 ms at the longest in a
// release build, through the room's timer and video tests, so going ahead
// of the game takes little from it.
#[allow(unsafe_code)]
pub fn raise_priority() -> io::Result<()> {
    // SAFETY: GetCurrentThread returns a pseudo handle for the calling
    // thread, which needs no closing.
    if unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) } == 0 {
        let err = io::Error::last_os_error();
        return Err(io::Error::new(
            err.kind(),
            format!("could not raise the thread's priority: {err}"),
        ));
    }
    Ok(())
}

fn run(shared: &Shared, timer: &Timer, mut send: impl FnMut(&[u8])) {
    // Failing to raise it only costs a spread that stretches on a busy PC.
    let _ = raise_priority();
    let spreads = timer.high_resolution();
    let mut going: Option<Going> = None;
    loop {
        if let Err(err) = wait(&shared.signal, going.as_ref().map(|_| timer)) {
            fail(shared, going.take(), &mut send, err);
            return;
        }
        let (job, stop) = {
            let mut mailbox = shared.lock();
            (mailbox.job.take(), mailbox.stop)
        };
        if stop {
            return;
        }
        let now = Instant::now();
        if let Some(job) = job {
            if let Some(mut old) = going.take() {
                old.send_rest(&mut send, shared);
                shared.cut_short.fetch_add(1, Ordering::Relaxed);
                recycle(shared, old.job.burst);
            }
            shared.frames.fetch_add(1, Ordering::Relaxed);
            going = Some(Going::new(job, now, spreads));
        }
        let Some(current) = going.as_mut() else {
            continue;
        };
        current.send_due(now, &mut send, shared);
        if current.done() {
            if let Some(done) = going.take() {
                recycle(shared, done.job.burst);
            }
        } else if let Err(err) = timer.set_at(current.next_due()) {
            fail(shared, going.take(), &mut send, err);
            return;
        }
    }
}

fn recycle(shared: &Shared, burst: Burst) {
    keep_spare(&mut shared.lock(), burst);
}

// Two spares cover the steady state: one burst being filled while one is
// sent. A few more are kept for frames dropped from the mailbox; the rest
// are let go.
fn keep_spare(mailbox: &mut Mailbox, burst: Burst) {
    if mailbox.spare.len() < 4 {
        mailbox.spare.push(burst);
    }
}

fn fail(shared: &Shared, going: Option<Going>, send: &mut impl FnMut(&[u8]), err: io::Error) {
    if let Some(mut going) = going {
        going.send_rest(send, shared);
    }
    stop_failed(shared, err.to_string());
}

// From here on put drops each frame it is given, and the frame that was
// waiting is dropped with it.
fn stop_failed(shared: &Shared, why: String) {
    let mut mailbox = shared.lock();
    mailbox.failed = Some(format!("the video send thread stopped: {why}"));
    mailbox.job = None;
}

fn panic_message(payload: &(dyn Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<&str>() {
        message
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message
    } else {
        "no message"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    // What a Windows before 10 1803 gets: waits end on the default timer's
    // tick, so a spread would run past half a 120 fps interval. Frames go at
    // once instead, and the pacer says why.
    #[test]
    fn fallback_timer_sends_at_once() {
        let timer = Timer::normal(None).unwrap();
        assert!(!timer.high_resolution());
        let mut late = Vec::new();
        for _ in 0..20 {
            let start = Instant::now();
            timer.set(Duration::from_millis(2)).unwrap();
            timer.wait().unwrap();
            late.push(start.elapsed().saturating_sub(Duration::from_millis(2)));
        }
        late.sort();
        println!(
            "fallback timer, 2 ms wait over 20: median {:?} late, worst {:?}",
            late[10], late[19]
        );

        let (sender, receiver) = mpsc::channel();
        let pacer = Pacer::start_with(timer, move |_: &[u8]| {
            let _ = sender.send(Instant::now());
        })
        .unwrap();
        let note = pacer.note().unwrap();
        assert!(note.contains("no high resolution timer"), "{note}");
        assert!(
            note.ends_with("video packets go out without spreading"),
            "{note}"
        );
        let mut burst = pacer.burst();
        for _ in 0..72 {
            burst.push(&[0; 1166]);
        }
        pacer.put(burst, Duration::from_nanos(8_333_333), true);
        let sent: Vec<Instant> = (0..72)
            .map(|_| receiver.recv_timeout(Duration::from_secs(5)).unwrap())
            .collect();
        assert!(sent[71] - sent[0] < Duration::from_millis(1));
    }

    #[test]
    fn burst_keeps_packets_whole() {
        let mut burst = Burst::new();
        assert!(burst.is_empty());
        burst.push(b"one");
        burst.push(b"");
        burst.push(b"three");
        assert_eq!(burst.len(), 3);
        assert_eq!(burst.packet(0), Some(&b"one"[..]));
        assert_eq!(burst.packet(1), Some(&b""[..]));
        assert_eq!(burst.packet(2), Some(&b"three"[..]));
        assert_eq!(burst.packet(3), None);
        burst.clear();
        assert_eq!(burst.packet(0), None);
    }

    fn burst_of(pacer: &Pacer, number: u8, count: u8) -> Burst {
        let mut burst = pacer.burst();
        for index in 0..count {
            burst.push(&[number, index]);
        }
        burst
    }

    fn waiting(pacer: &Pacer) -> Option<usize> {
        pacer.shared.lock().job.as_ref().map(|job| job.burst.len())
    }

    // However long the thread is held up, the mailbox holds the newest frame
    // and nothing else.
    #[test]
    fn held_up_thread_keeps_one_frame() {
        let (gate, opened) = mpsc::channel::<()>();
        let (reached, held) = mpsc::channel::<()>();
        let pacer = Pacer::start(move |packet: &[u8]| {
            if packet == [1, 0] {
                let _ = reached.send(());
                let _ = opened.recv();
            }
        })
        .unwrap();
        pacer.put(
            burst_of(&pacer, 1, 2),
            Duration::from_nanos(8_333_333),
            false,
        );
        held.recv_timeout(Duration::from_secs(5)).unwrap();
        for number in 2..=101 {
            pacer.put(
                burst_of(&pacer, number, 30),
                Duration::from_nanos(8_333_333),
                false,
            );
            assert_eq!(waiting(&pacer), Some(30));
        }
        assert_eq!(pacer.numbers().discarded, 99);
        assert!(pacer.shared.lock().spare.len() <= 4);
        gate.send(()).unwrap();
    }

    // The send function is the caller's code. When it panics the thread ends,
    // and the pacer says so and stops holding frames for it.
    #[test]
    fn panicking_send_function() {
        let (sender, receiver) = mpsc::channel();
        let pacer = Pacer::start(move |packet: &[u8]| {
            if packet[0] == 2 {
                panic!("could not seal packet {}", packet[1]);
            }
            let _ = sender.send(packet[0]);
        })
        .unwrap();
        let interval = Duration::from_nanos(8_333_333);
        pacer.put(burst_of(&pacer, 1, 3), interval, false);
        for _ in 0..3 {
            assert_eq!(receiver.recv_timeout(Duration::from_secs(5)), Ok(1));
        }
        pacer.put(burst_of(&pacer, 2, 3), interval, false);
        // The sender goes with the closure as the thread unwinds; the note
        // is written just after.
        assert!(receiver.recv_timeout(Duration::from_secs(5)).is_err());
        let deadline = Instant::now() + Duration::from_secs(5);
        let failure = loop {
            if let Some(failure) = pacer.failure() {
                break failure;
            }
            assert!(Instant::now() < deadline, "no failure after the panic");
            thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(
            failure,
            "the video send thread stopped: it panicked: could not seal packet 0"
        );
        for number in 3..50 {
            pacer.put(burst_of(&pacer, number, 30), interval, true);
            assert_eq!(waiting(&pacer), None);
        }
        assert_eq!(pacer.numbers().frames, 2);
    }
}
