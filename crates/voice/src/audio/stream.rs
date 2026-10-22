// The capture and render threads. They run the same way whatever is behind
// the Device trait: Windows in the app, a fake in the tests. One thread per
// stream, blocking on the device's event; the timeout on that wait is there
// only so a stop request is seen when the device has gone quiet.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError};

use super::devices::{Choice, Direction};
use super::echo::{Echo, Effects};
use super::error::AudioError;
use super::format::{Format, RATE, from_mono, interleaved, to_mono};
use super::level::{Level, Meter, Reading};
use super::wasapi;

// A stop is seen within this when the device sends nothing, and within one
// period when it runs.
const WAIT: Duration = Duration::from_millis(20);
// After a device under Windows default goes away. Bluetooth headsets drop
// and come back in about this time when they switch profiles, and Windows
// needs a moment to pick the next default.
pub const REOPEN_AFTER: Duration = Duration::from_secs(1);
// Windows' process loopback, which PC sound needs, comes with this build.
pub const PC_SOUND_BUILD: u32 = 20348;
// A device chosen in settings that is away is tried again whenever Windows
// says the devices changed, and at least this often: a format change in the
// sound settings ends a stream with no such notice, and a stream whose
// watch could not be registered gets none at all.
pub const LOOK_AGAIN: Duration = Duration::from_secs(2);
// The queued average follows the last 16 or so writes.
const QUEUED_SHIFT: u32 = 4;
// A stereo packet stamped this much after the last one ended comes after a
// stretch with nothing: Windows sent no packets while nothing played.
const GAP: Duration = Duration::from_millis(5);
// A packet with no stamp is taken to follow the one before, unless that puts
// it further than this from when it can have been captured.
const DRIFT: Duration = Duration::from_millis(20);

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
    // When Windows refused the first ask and the stream opened on a second.
    pub refused: Option<Refused>,
    // What Windows does to a microphone's sound before Booth gets it.
    pub effects: Effects,
    pub echo: Echo,
    // How long the open took, a Bluetooth headset's change of profile or
    // process loopback's activation included.
    pub opened_in: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refused {
    // The engine format the first ask was made from.
    pub engine: Format,
    pub code: i32,
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
    // The device chosen in settings is not there: unplugged or turned off
    // when the stream opened, or since. Said once; the stream opens it again
    // once it is back, with Opened.
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
    // A device was added, removed, turned on or off.
    DevicesChanged,
}

// Handed to a device when the thread starts, so a notification from Windows
// can reach the thread.
#[derive(Clone)]
pub struct Wake(Sender<Control>);

impl Wake {
    pub fn default_changed(&self, id: Option<String>) {
        // The thread may have ended; nothing is waiting then.
        self.send(Control::DefaultChanged(id));
    }

    pub fn devices_changed(&self) {
        self.send(Control::DevicesChanged);
    }

    // False once the thread has ended.
    pub(crate) fn send(&self, control: Control) -> bool {
        self.0.send(control).is_ok()
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
    // Stereo only: reads after Windows set the event, and after a wait it
    // did not.
    event_reads: AtomicU64,
    quiet_reads: AtomicU64,
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
    // Set at stop as well, for a device whose open waits on Windows and
    // looks at it meanwhile.
    stopping: Option<Arc<AtomicBool>>,
}

impl Handle {
    fn stop(&mut self) {
        self.stopping();
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
        self.stopping();
        let _ = self.control.send(Control::Stop);
        self.thread = None;
        self.outside()
    }

    fn stopping(&self) {
        if let Some(stopping) = &self.stopping {
            stopping.store(true, Ordering::Relaxed);
        }
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
type StereoIn = Box<dyn FnMut(&[f32], Instant, bool) + Send>;
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

    // As start, with Windows' echo cancellation where the microphone has it
    // (see wasapi.rs), listening to `reference`, the output device Booth
    // plays to, or to Windows default with None. The stream info says what
    // came of it.
    pub fn start_cancelling_echo(
        choice: Choice,
        reference: Option<String>,
        audio: impl FnMut(&[f32], Instant) + Send + 'static,
        events: impl FnMut(Event) + Send + 'static,
    ) -> Result<Capture, AudioError> {
        let make =
            move |direction, wake| wasapi::device_cancelling_echo(direction, wake, reference);
        Capture::start_with(make, choice, audio, events)
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
        let name = format!("audio {}", Direction::Input);
        spawn(name, Direction::Input, make, choice, work, Box::new(events)).map(Capture)
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
        let name = format!("audio {}", Direction::Output);
        spawn(
            name,
            Direction::Output,
            make,
            choice,
            work,
            Box::new(events),
        )
        .map(Render)
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

// PC sound: what the other programs on this PC play, as interleaved stereo
// f32 at 48 kHz, with the time the first frame of each packet was captured
// and whether the packet comes after a gap: the first after an open, sound
// Windows lost, or a stretch with nothing. Stops when dropped.
pub struct PcSound(Handle);

impl PcSound {
    // From Windows' process loopback (see wasapi.rs), which leaves this
    // process and every process it started out. Builds before PC_SOUND_BUILD
    // have none, and the open fails there.
    pub fn start(
        audio: impl FnMut(&[f32], Instant, bool) + Send + 'static,
        events: impl FnMut(Event) + Send + 'static,
    ) -> Result<PcSound, AudioError> {
        let stopping = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stopping);
        let make = move |_, _| wasapi::pc_sound_device(flag);
        let mut sound = PcSound::start_with(make, audio, events)?;
        sound.0.stopping = Some(stopping);
        Ok(sound)
    }

    // `make` runs on the new thread, as for Capture::start_with. The device
    // has to give two channels.
    pub fn start_with<D, F>(
        make: F,
        audio: impl FnMut(&[f32], Instant, bool) + Send + 'static,
        events: impl FnMut(Event) + Send + 'static,
    ) -> Result<PcSound, AudioError>
    where
        D: Device,
        F: FnOnce(Direction, Wake) -> Result<D, AudioError> + Send + 'static,
    {
        let work = Work::Stereo {
            audio: Box::new(audio),
            meter: Meter::default(),
            stereo: Vec::new(),
            mid: Vec::new(),
            first: true,
            end: None,
        };
        let name = String::from("audio pc sound");
        spawn(
            name,
            Direction::Input,
            make,
            Choice::Default,
            work,
            Box::new(events),
        )
        .map(PcSound)
    }

    pub fn info(&self) -> Option<StreamInfo> {
        self.0.shared.info()
    }

    // Of both channels together.
    pub fn level(&self) -> Reading {
        self.0.shared.level.read()
    }

    pub fn glitches(&self) -> u64 {
        self.0.shared.glitches.load(Ordering::Relaxed)
    }

    // Reads after Windows set the stream's event, and reads after a wait it
    // did not, over every stream this has opened.
    pub fn reads(&self) -> (u64, u64) {
        (
            self.0.shared.event_reads.load(Ordering::Relaxed),
            self.0.shared.quiet_reads.load(Ordering::Relaxed),
        )
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

// When a stereo packet's first frame was captured: its stamp, or with none,
// where the packet before ended, unless that is more than DRIFT from now less
// the packet's length, the latest it can have been captured.
fn packet_time(
    stamp: Option<Instant>,
    end: Option<Instant>,
    now: Instant,
    length: Duration,
) -> Instant {
    if let Some(stamp) = stamp {
        return stamp;
    }
    let latest = now.checked_sub(length).unwrap_or(now);
    match end {
        Some(end) if end.duration_since(latest).max(latest.duration_since(end)) <= DRIFT => end,
        _ => latest,
    }
}

// Only a packet that starts late is after a gap: one stamped early is
// Windows' clock and the packet clock disagreeing by a little.
fn after_gap(time: Instant, end: Option<Instant>) -> bool {
    end.is_some_and(|end| time > end + GAP)
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
    Stereo {
        audio: StereoIn,
        meter: Meter,
        stereo: Vec<f32>,
        // Both channels' average, for the meter.
        mid: Vec<f32>,
        first: bool,
        // Where the last packet ended, on its own clock.
        end: Option<Instant>,
    },
}

impl Work {
    // Process loopback can have packets ready while its event stays quiet,
    // so it is read on every wait; the other streams are only checked.
    fn reads_unwoken(&self) -> bool {
        matches!(self, Work::Stereo { .. })
    }

    fn start(&mut self, stream: &mut dyn DeviceStream) -> Result<(), AudioError> {
        match self {
            Work::Capture { first, .. } => *first = true,
            Work::Stereo { first, end, .. } => {
                *first = true;
                *end = None;
            }
            Work::Render { .. } => {
                // Silence first, so the device has something to play until
                // the first wake.
                let frames = stream.info().render_target();
                stream.write(frames, &mut |bytes| bytes.fill(0))?;
            }
        }
        stream.start()
    }

    fn gauge(&self) {
        match self {
            Work::Capture { mono, .. } => stats::gauge!("voice.capture.mono", *mono),
            Work::Render { mono, .. } => stats::gauge!("voice.render.mono", *mono),
            Work::Stereo { stereo, mid, .. } => {
                stats::gauge!("voice.pc_sound.stereo", *stereo);
                stats::gauge!("voice.pc_sound.mid", *mid);
            }
        }
    }

    fn serve(&mut self, stream: &mut dyn DeviceStream, shared: &Shared) -> Result<(), AudioError> {
        self.gauge();
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
            Work::Stereo {
                audio,
                meter,
                stereo,
                mid,
                first,
                end,
            } => stream.read(&mut |packet| {
                if packet.glitch && !*first {
                    shared.glitches.fetch_add(1, Ordering::Relaxed);
                }
                stereo.clear();
                if packet.silent {
                    stereo.resize(packet.frames as usize * 2, 0.0);
                } else {
                    interleaved(packet.data, &format, stereo);
                }
                mid.clear();
                let (pairs, _) = stereo.as_chunks::<2>();
                mid.extend(pairs.iter().map(|[left, right]| (left + right) * 0.5));
                meter.push(mid, &shared.level);
                let length = frames_to_time(packet.frames);
                let time = packet_time(packet.time, *end, Instant::now(), length);
                let gap = *first || packet.glitch || after_gap(time, *end);
                *first = false;
                *end = Some(time + length);
                audio(stereo, time, gap);
            }),
        }
    }
}

fn frames_to_time(frames: u32) -> Duration {
    Duration::from_secs_f64(f64::from(frames) / f64::from(RATE))
}

fn spawn<D, F>(
    name: String,
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
        .name(name)
        .spawn(move || {
            let raised = wasapi::raise_thread();
            match make(direction, wake) {
                Ok(mut device) => {
                    let run = Run {
                        choice: &choice,
                        rx: &rx,
                        shared: &thread_shared,
                        pro_audio: raised.is_some(),
                        notices: Cell::new(0),
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
        stopping: None,
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
    // Devices-changed notices seen so far, whichever wait read them, so one
    // that came while a device was being opened still counts once the open
    // has found it away.
    notices: Cell<u64>,
}

impl Run<'_> {
    fn run<D: Device>(&self, device: &mut D, work: &mut Work, events: &mut dyn FnMut(Event)) {
        // A chosen device that is away has said so, and says nothing more
        // until it opens again.
        let mut away = false;
        loop {
            let seen = self.notices.get();
            let opening = Instant::now();
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
                // A device chosen in settings that is unplugged or turned off
                // opens once it is back, as Windows default does.
                Err(err) if *self.choice != Choice::Default && err.is_away() => {
                    if !away {
                        away = true;
                        events(Event::Gone(err));
                    }
                    if self.wait_for_devices(seen) {
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
            info.opened_in = opening.elapsed();
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
            if work.reads_unwoken() && info.format.channels != 2 {
                events(Event::Failed(AudioError::PcSound {
                    code: 0,
                    text: format!(
                        "Windows gave it as {} channels where Booth asked for 2",
                        info.format.channels
                    ),
                    lost: false,
                }));
                return;
            }
            let end = match work.start(&mut stream) {
                Ok(()) => {
                    away = false;
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
                    // Opened again as soon as Windows says the devices
                    // changed since this open. The unplug that ended it
                    // usually has already, so the next open comes at once
                    // and finds a quick replug.
                    Choice::Device(_) if err.is_lost() => {
                        if !away {
                            away = true;
                            events(Event::Gone(err));
                        }
                        if !self.wait_for_devices(seen) {
                            return;
                        }
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
            if work.reads_unwoken() {
                let reads = if signalled {
                    &self.shared.event_reads
                } else {
                    &self.shared.quiet_reads
                };
                reads.fetch_add(1, Ordering::Relaxed);
            }
            // The Windows default also has the default-changed notice when
            // it goes; a device chosen in settings has only this.
            let result = if signalled || work.reads_unwoken() {
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
                Ok(Control::DevicesChanged) => self.noticed(),
                Err(TryRecvError::Empty) => return None,
            }
        }
    }

    fn noticed(&self) {
        self.notices.set(self.notices.get() + 1);
    }

    // False when asked to stop.
    fn pause(&self, how_long: Duration) -> bool {
        let until = Instant::now() + how_long;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(Control::Stop) | Err(RecvTimeoutError::Disconnected) => return false,
                Ok(Control::DefaultChanged(_)) => {}
                Ok(Control::DevicesChanged) => self.noticed(),
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
                Ok(Control::DevicesChanged) => self.noticed(),
                Ok(Control::Stop) | Err(_) => return false,
            }
        }
    }

    // A chosen device found away by the open made after `seen` notices:
    // until Windows says the devices changed since, or LOOK_AGAIN. False when
    // asked to stop.
    fn wait_for_devices(&self, seen: u64) -> bool {
        let until = Instant::now() + LOOK_AGAIN;
        while self.notices.get() == seen {
            let left = until.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(Control::Stop) | Err(RecvTimeoutError::Disconnected) => return false,
                Ok(Control::DefaultChanged(_)) => {}
                Ok(Control::DevicesChanged) => self.noticed(),
                Err(RecvTimeoutError::Timeout) => return true,
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEN_MS: Duration = Duration::from_millis(10);

    #[test]
    fn a_stamp_is_kept_and_a_missing_one_follows_the_last_packet() {
        let now = Instant::now() + Duration::from_secs(10);
        let stamp = now - Duration::from_millis(37);
        assert_eq!(packet_time(Some(stamp), None, now, TEN_MS), stamp);
        assert_eq!(packet_time(Some(stamp), Some(now), now, TEN_MS), stamp);
        // No stamp: where the last one ended, within DRIFT either way of now
        // less the packet's length.
        let latest = now - TEN_MS;
        for off in [Duration::ZERO, DRIFT] {
            assert_eq!(
                packet_time(None, Some(latest - off), now, TEN_MS),
                latest - off
            );
            assert_eq!(
                packet_time(None, Some(latest + off), now, TEN_MS),
                latest + off
            );
        }
        // Further than that, as after seconds with no packets, or with none
        // before it: now less the packet's length.
        let long_ago = latest - DRIFT - Duration::from_micros(1);
        assert_eq!(packet_time(None, Some(long_ago), now, TEN_MS), latest);
        assert_eq!(packet_time(None, Some(now + DRIFT), now, TEN_MS), latest);
        assert_eq!(packet_time(None, None, now, TEN_MS), latest);
    }

    #[test]
    fn only_a_late_packet_comes_after_a_gap() {
        let end = Instant::now() + Duration::from_secs(10);
        assert!(!after_gap(end, None));
        assert!(!after_gap(end, Some(end)));
        assert!(!after_gap(end + GAP, Some(end)));
        assert!(after_gap(end + GAP + Duration::from_micros(1), Some(end)));
        // Early is the two clocks disagreeing, never a gap.
        assert!(!after_gap(end - Duration::from_millis(50), Some(end)));
    }
}
