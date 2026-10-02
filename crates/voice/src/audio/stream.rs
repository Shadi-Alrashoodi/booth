// The capture and render threads. They run the same way whatever is behind
// the Device trait: Windows in the app, a fake in the tests. One thread per
// stream, blocking on the device's event; the timeout on that wait is there
// only so a stop request is seen when the device has gone quiet.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError};

use super::devices::{Choice, Direction};
use super::error::AudioError;
use super::format::{Format, RATE, from_mono, to_mono};
use super::level::{Level, Meter, Reading};
use super::wasapi;

// A stop is seen within this when the device sends nothing, and within one
// period when it runs.
const WAIT: Duration = Duration::from_millis(20);
// After a device under Windows default goes away. Bluetooth headsets drop
// and come back in about this time when they switch profiles, and Windows
// needs a moment to pick the next default.
pub const REOPEN_AFTER: Duration = Duration::from_secs(1);
// The queued average follows the last 16 or so writes.
const QUEUED_SHIFT: u32 = 4;

// What the thread asks of a device. Windows implements it in wasapi.rs.
pub trait Device {
    type Stream: DeviceStream;
    fn open(&mut self, choice: &Choice) -> Result<Self::Stream, AudioError>;
}

// One opened stream in shared mode, event driven. Capture streams use read,
// render streams queued and write.
pub trait DeviceStream {
    fn info(&self) -> &StreamInfo;
    fn start(&mut self) -> Result<(), AudioError>;
    // True when the device signalled before `timeout` ran out.
    fn wait(&mut self, timeout: Duration) -> Result<bool, AudioError>;
    // Every packet that is ready, oldest first.
    fn read(&mut self, packet: &mut dyn FnMut(Packet<'_>)) -> Result<(), AudioError>;
    // Frames written and not yet played.
    fn queued(&mut self) -> Result<u32, AudioError>;
    // Fails once the device has gone away. A device that is gone stops
    // setting its event, so a quiet one has to be asked.
    fn check(&mut self) -> Result<(), AudioError>;
    // `frames` frames in the stream's format, filled in by `fill`.
    fn write(&mut self, frames: u32, fill: &mut dyn FnMut(&mut [u8])) -> Result<(), AudioError>;
    fn stop(&mut self);
}

pub struct Packet<'a> {
    // `frames` frames in the stream's format.
    pub data: &'a [u8],
    pub frames: u32,
    // When the first frame was captured, from the device's own timestamp.
    // None when the device said its timestamp was wrong.
    pub time: Option<Instant>,
    // Windows says the packet is silence; `data` may hold anything.
    pub silent: bool,
    // Windows says sound was lost before this packet, most often because
    // the thread was too late to read it.
    pub glitch: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StreamInfo {
    pub direction: Direction,
    pub device: String,
    pub id: String,
    // What the stream delivers or takes: always 48 kHz.
    pub format: Format,
    // What the Windows audio engine runs the device at.
    pub engine: Format,
    // A Bluetooth headset in hands-free mode, from the endpoint's properties.
    pub hands_free: bool,
    pub period: Duration,
    // The same, in frames of the stream.
    pub period_frames: u32,
    // Windows converts from the engine's rate to 48 kHz for this stream.
    pub resampled: bool,
    // Opened with the Windows 10 call that allows periods under 10 ms.
    pub small_period: bool,
    // What Windows reports as the stream's own latency on top of the buffer.
    pub stream_latency: Duration,
    pub buffer_frames: u32,
    // The thread got the "Pro Audio" scheduling class.
    pub pro_audio: bool,
}

impl StreamInfo {
    pub fn period_ms(&self) -> f64 {
        self.period.as_secs_f64() * 1000.0
    }

    // How many frames a render stream keeps queued after each write: two
    // periods, so that when the device wakes the thread one period is still
    // there to play while the next is written. Windows' own buffer is often
    // larger (22 ms against a 10 ms period on Bluetooth), and filling all of
    // it would only add delay.
    pub fn render_target(&self) -> u32 {
        self.buffer_frames.min(self.period_frames.saturating_mul(2))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Opened(StreamInfo),
    // The device went away or failed while the stream ran, under Windows
    // default. The stream opens again on the default in REOPEN_AFTER.
    Lost(AudioError),
    // The device chosen in settings went away. The stream has ended.
    Gone(AudioError),
    // Could not open, or failed on a chosen device. The stream has ended,
    // except under Windows default with no device at all, where it opens as
    // soon as Windows has a default again.
    Failed(AudioError),
}

pub(crate) enum Control {
    Stop,
    // The Windows default changed to this endpoint, or to none.
    DefaultChanged(Option<String>),
}

// Handed to a device when the thread starts, so a notification from Windows
// can reach the thread.
#[derive(Clone)]
pub struct Wake(Sender<Control>);

impl Wake {
    pub fn default_changed(&self, id: Option<String>) {
        // The thread may have ended; nothing is waiting then.
        let _ = self.0.send(Control::DefaultChanged(id));
    }
}

#[derive(Default)]
struct Shared {
    info: Mutex<Option<StreamInfo>>,
    level: Level,
    // Frames queued ahead of each write, averaged, times 2^QUEUED_SHIFT.
    queued: AtomicU32,
    // Wakes that found nothing left to play: the device ran dry and played
    // a gap.
    underruns: AtomicU64,
    // Captured packets that came after lost sound.
    glitches: AtomicU64,
}

impl Shared {
    fn set_info(&self, info: Option<StreamInfo>) {
        *self.info.lock().unwrap_or_else(PoisonError::into_inner) = info;
    }

    fn info(&self) -> Option<StreamInfo> {
        self.info
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn note_queued(&self, frames: u32) {
        let scaled = frames << QUEUED_SHIFT;
        let old = self.queued.load(Ordering::Relaxed);
        let new = if old == 0 {
            scaled
        } else {
            old - (old >> QUEUED_SHIFT) + (scaled >> QUEUED_SHIFT)
        };
        self.queued.store(new, Ordering::Relaxed);
    }
}

struct Handle {
    control: Sender<Control>,
    thread: Option<JoinHandle<()>>,
    // Nothing is ever sent on it. It disconnects when the thread returns,
    // after it let go of the device and dropped the owner's callbacks.
    ended: Receiver<()>,
    shared: Arc<Shared>,
}

impl Handle {
    fn stop(&mut self) {
        let _ = self.control.send(Control::Stop);
        if let Some(thread) = self.thread.take() {
            // A panic in a callback ends the thread; there is nothing left
            // to stop then, and the owner's own code already said why.
            let _ = thread.join();
        }
    }

    fn outside(&self) -> StreamThread {
        StreamThread {
            ended: self.ended.clone(),
            shared: Arc::clone(&self.shared),
        }
    }

    fn let_go(mut self) -> StreamThread {
        let _ = self.control.send(Control::Stop);
        self.thread = None;
        self.outside()
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop();
    }
}

// A stream's thread as its owner sees it, for as long as the thread runs,
// which can be well after a Capture or Render let it go: whether it has
// ended, and the device it has open meanwhile.
#[derive(Clone)]
pub struct StreamThread {
    ended: Receiver<()>,
    shared: Arc<Shared>,
}

impl StreamThread {
    // Disconnects once the thread has ended, its device let go and the
    // callbacks dropped. Nothing is sent on it.
    pub fn ended(&self) -> &Receiver<()> {
        &self.ended
    }

    pub fn running(&self) -> bool {
        !matches!(self.ended.try_recv(), Err(TryRecvError::Disconnected))
    }

    // The id of the device the stream has open, closing included. None
    // while it opens one, waits to open one again, or has closed.
    pub fn device(&self) -> Option<String> {
        self.shared.info().map(|info| info.id)
    }
}

type AudioIn = Box<dyn FnMut(&[f32], Instant) + Send>;
type AudioOut = Box<dyn FnMut(&mut [f32]) + Send>;
type Events = Box<dyn FnMut(Event) + Send>;

// A microphone, delivering mono f32 at 48 kHz with the time the first sample
// of each packet was captured. Stops when dropped.
pub struct Capture(Handle);

impl Capture {
    pub fn start(
        choice: Choice,
        audio: impl FnMut(&[f32], Instant) + Send + 'static,
        events: impl FnMut(Event) + Send + 'static,
    ) -> Result<Capture, AudioError> {
        Capture::start_with(wasapi::device, choice, audio, events)
    }

    // `make` runs on the new thread, which is where the device has to live.
    pub fn start_with<D, F>(
        make: F,
        choice: Choice,
        audio: impl FnMut(&[f32], Instant) + Send + 'static,
        events: impl FnMut(Event) + Send + 'static,
    ) -> Result<Capture, AudioError>
    where
        D: Device,
        F: FnOnce(Direction, Wake) -> Result<D, AudioError> + Send + 'static,
    {
        let work = Work::Capture {
            audio: Box::new(audio),
            meter: Meter::default(),
            mono: Vec::new(),
            first: true,
        };
        spawn(Direction::Input, make, choice, work, Box::new(events)).map(Capture)
    }

    pub fn info(&self) -> Option<StreamInfo> {
        self.0.shared.info()
    }

    pub fn level(&self) -> Reading {
        self.0.shared.level.read()
    }

    // Times Windows lost captured sound, over every stream this has opened.
    pub fn glitches(&self) -> u64 {
        self.0.shared.glitches.load(Ordering::Relaxed)
    }

    pub fn thread(&self) -> StreamThread {
        self.0.outside()
    }

    // Returns once the stream's thread has ended.
    pub fn stop(mut self) {
        self.0.stop();
    }

    // Asks the thread to stop and returns at once. A device slow to close
    // keeps only that thread, never the caller.
    pub fn let_go(self) -> StreamThread {
        self.0.let_go()
    }
}

// Speakers or headphones. The callback fills exactly the frames the device
// asks for, in mono; every channel gets the same samples. Stops when dropped.
pub struct Render(Handle);

impl Render {
    pub fn start(
        choice: Choice,
        fill: impl FnMut(&mut [f32]) + Send + 'static,
        events: impl FnMut(Event) + Send + 'static,
    ) -> Result<Render, AudioError> {
        Render::start_with(wasapi::device, choice, fill, events)
    }

    pub fn start_with<D, F>(
        make: F,
        choice: Choice,
        fill: impl FnMut(&mut [f32]) + Send + 'static,
        events: impl FnMut(Event) + Send + 'static,
    ) -> Result<Render, AudioError>
    where
        D: Device,
        F: FnOnce(Direction, Wake) -> Result<D, AudioError> + Send + 'static,
    {
        let work = Work::Render {
            fill: Box::new(fill),
            mono: Vec::new(),
        };
        spawn(Direction::Output, make, choice, work, Box::new(events)).map(Render)
    }

    pub fn info(&self) -> Option<StreamInfo> {
        self.0.shared.info()
    }

    // The render side's delay: one period, the stream latency Windows
    // reports, and what was already queued when new samples went in,
    // averaged over the last writes.
    pub fn latency(&self) -> Option<Duration> {
        let info = self.0.shared.info()?;
        Some(info.period + info.stream_latency + self.queued()?)
    }

    pub fn underruns(&self) -> u64 {
        self.0.shared.underruns.load(Ordering::Relaxed)
    }

    pub fn queued(&self) -> Option<Duration> {
        self.0.shared.info()?;
        let scaled = self.0.shared.queued.load(Ordering::Relaxed);
        let frames = f64::from(scaled) / f64::from(1u32 << QUEUED_SHIFT);
        Some(Duration::from_secs_f64(frames / f64::from(RATE)))
    }

    pub fn thread(&self) -> StreamThread {
        self.0.outside()
    }

    pub fn stop(mut self) {
        self.0.stop();
    }

    // As Capture's.
    pub fn let_go(self) -> StreamThread {
        self.0.let_go()
    }
}

enum Work {
    Capture {
        audio: AudioIn,
        meter: Meter,
        mono: Vec<f32>,
        // Windows may mark the first packet after a start as lost sound,
        // since the stream has just changed state. That one is not counted.
        first: bool,
    },
    Render {
        fill: AudioOut,
        mono: Vec<f32>,
    },
}

impl Work {
    fn start(&mut self, stream: &mut dyn DeviceStream) -> Result<(), AudioError> {
        match self {
            Work::Capture { first, .. } => *first = true,
            Work::Render { .. } => {
                // Silence first, so the device has something to play until
                // the first wake.
                let frames = stream.info().render_target();
                stream.write(frames, &mut |bytes| bytes.fill(0))?;
            }
        }
        stream.start()
    }

    fn serve(&mut self, stream: &mut dyn DeviceStream, shared: &Shared) -> Result<(), AudioError> {
        let format = stream.info().format;
        match self {
            Work::Capture {
                audio,
                meter,
                mono,
                first,
            } => stream.read(&mut |packet| {
                if packet.glitch && !*first {
                    shared.glitches.fetch_add(1, Ordering::Relaxed);
                }
                *first = false;
                mono.clear();
                if packet.silent {
                    mono.resize(packet.frames as usize, 0.0);
                } else {
                    to_mono(packet.data, &format, mono);
                }
                meter.push(mono, &shared.level);
                let time = packet
                    .time
                    .unwrap_or_else(|| Instant::now() - frames_to_time(packet.frames));
                audio(mono, time);
            }),
            Work::Render { fill, mono } => {
                let target = stream.info().render_target();
                let queued = stream.queued()?;
                let frames = target.saturating_sub(queued);
                if queued == 0 {
                    shared.underruns.fetch_add(1, Ordering::Relaxed);
                }
                if frames == 0 {
                    return Ok(());
                }
                mono.clear();
                mono.resize(frames as usize, 0.0);
                fill(mono);
                stream.write(frames, &mut |bytes| from_mono(mono, &format, bytes))?;
                shared.note_queued(queued);
                Ok(())
            }
        }
    }
}

fn frames_to_time(frames: u32) -> Duration {
    Duration::from_secs_f64(f64::from(frames) / f64::from(RATE))
}

fn spawn<D, F>(
    direction: Direction,
    make: F,
    choice: Choice,
    mut work: Work,
    mut events: Events,
) -> Result<Handle, AudioError>
where
    D: Device,
    F: FnOnce(Direction, Wake) -> Result<D, AudioError> + Send + 'static,
{
    let (control, rx) = crossbeam_channel::unbounded();
    let (finished, ended) = crossbeam_channel::bounded::<()>(0);
    let wake = Wake(control.clone());
    let shared = Arc::new(Shared::default());
    let thread_shared = Arc::clone(&shared);
    let thread = thread::Builder::new()
        .name(format!("audio {direction}"))
        .spawn(move || {
            let raised = wasapi::raise_thread();
            match make(direction, wake) {
                Ok(mut device) => {
                    let run = Run {
                        choice: &choice,
                        rx: &rx,
                        shared: &thread_shared,
                        pro_audio: raised.is_some(),
                    };
                    run.run(&mut device, &mut work, &mut *events);
                }
                Err(err) => events(Event::Failed(err)),
            }
            drop(raised);
            // Whoever waits on `ended` finds the device let go and nothing
            // of the owner's left on this thread.
            drop((work, events));
            drop(finished);
        })
        .map_err(|err| AudioError::Thread(err.to_string()))?;
    Ok(Handle {
        control,
        thread: Some(thread),
        ended,
        shared,
    })
}

enum End {
    Stop,
    // Windows default moved to another device.
    Switch,
    Error(AudioError),
}

struct Run<'a> {
    choice: &'a Choice,
    rx: &'a Receiver<Control>,
    shared: &'a Shared,
    pro_audio: bool,
}

impl Run<'_> {
    fn run<D: Device>(&self, device: &mut D, work: &mut Work, events: &mut dyn FnMut(Event)) {
        loop {
            let mut stream = match device.open(self.choice) {
                Ok(stream) => stream,
                // A default that vanished while it was being opened, as a
                // Bluetooth headset does when it switches profiles, is a
                // loss like any other.
                Err(err) if *self.choice == Choice::Default && err.is_lost() => {
                    events(Event::Lost(err));
                    if self.pause(REOPEN_AFTER) {
                        continue;
                    }
                    return;
                }
                Err(err) => {
                    let wait =
                        *self.choice == Choice::Default && matches!(err, AudioError::NoDevice(_));
                    events(Event::Failed(err));
                    if wait && self.wait_for_default() {
                        continue;
                    }
                    return;
                }
            };
            let mut info = stream.info().clone();
            info.pro_audio = self.pro_audio;
            // Opening a Bluetooth headset can take a second while it changes
            // profile, and the owner may have asked to stop meanwhile. The
            // stream then ends here, before it starts.
            match self.controls(&info.id) {
                Some(End::Stop) => return,
                Some(End::Switch) => continue,
                Some(End::Error(_)) | None => {}
            }
            if info.format.rate != RATE {
                events(Event::Failed(AudioError::Rate {
                    direction: info.direction,
                    name: info.device.clone(),
                    rate: info.format.rate,
                }));
                return;
            }
            let end = match work.start(&mut stream) {
                Ok(()) => {
                    self.shared.set_info(Some(info.clone()));
                    events(Event::Opened(info.clone()));
                    self.pump(&mut stream, work, &info.id)
                }
                Err(err) => End::Error(err),
            };
            stream.stop();
            drop(stream);
            self.shared.set_info(None);
            self.shared.level.clear();
            self.shared.queued.store(0, Ordering::Relaxed);
            match end {
                End::Stop => return,
                End::Switch => {}
                End::Error(err) => match self.choice {
                    Choice::Default => {
                        events(Event::Lost(err));
                        if !self.pause(REOPEN_AFTER) {
                            return;
                        }
                    }
                    Choice::Device(_) if err.is_lost() => {
                        events(Event::Gone(err));
                        return;
                    }
                    Choice::Device(_) => {
                        events(Event::Failed(err));
                        return;
                    }
                },
            }
        }
    }

    fn pump(&self, stream: &mut dyn DeviceStream, work: &mut Work, id: &str) -> End {
        loop {
            let signalled = match stream.wait(WAIT) {
                Ok(signalled) => signalled,
                Err(err) => return End::Error(err),
            };
            if let Some(end) = self.controls(id) {
                return end;
            }
            // The Windows default also has the default-changed notice when
            // it goes; a device chosen in settings has only this.
            let result = if signalled {
                work.serve(stream, self.shared)
            } else {
                stream.check()
            };
            if let Err(err) = result {
                return End::Error(err);
            }
        }
    }

    // What the owner and Windows sent since the last look, for a stream open
    // on `id`. None while nothing ends it.
    fn controls(&self, id: &str) -> Option<End> {
        loop {
            match self.rx.try_recv() {
                Ok(Control::Stop) | Err(TryRecvError::Disconnected) => return Some(End::Stop),
                Ok(Control::DefaultChanged(to)) => {
                    if *self.choice == Choice::Default && to.as_deref() != Some(id) {
                        return Some(End::Switch);
                    }
                }
                Err(TryRecvError::Empty) => return None,
            }
        }
    }

    // False when asked to stop.
    fn pause(&self, how_long: Duration) -> bool {
        let until = Instant::now() + how_long;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(Control::Stop) | Err(RecvTimeoutError::Disconnected) => return false,
                Ok(Control::DefaultChanged(_)) => {}
                Err(RecvTimeoutError::Timeout) => return true,
            }
        }
    }

    // With no device at all there is nothing to open until Windows names a
    // new default, so the thread sleeps until it does. False when asked to
    // stop.
    fn wait_for_default(&self) -> bool {
        loop {
            match self.rx.recv() {
                Ok(Control::DefaultChanged(Some(_))) => return true,
                Ok(Control::DefaultChanged(None)) => {}
                Ok(Control::Stop) | Err(_) => return false,
            }
        }
    }
}
