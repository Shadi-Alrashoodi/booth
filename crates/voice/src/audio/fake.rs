// A device that exists only in memory, for testing the stream threads
// without a microphone or a speaker. It runs on the real clock: one period
// of frames becomes ready, or is played, every period.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use super::devices::{Choice, Direction};
use super::error::AudioError;
use super::format::{Format, Sample};
use super::stream::{Device, DeviceStream, Packet, StreamInfo, Wake};

#[derive(Clone, Copy, Debug)]
pub struct Setup {
    pub period_frames: u32,
    pub buffer_frames: u32,
    pub channels: u16,
    pub rate: u32,
    pub stream_latency: Duration,
    // Capture only: the value of one channel of one frame, counted from the
    // stream's first frame.
    pub signal: fn(frame: u64, channel: u16) -> f32,
    // How long each open takes, as a Bluetooth headset changing profile.
    pub open_time: Duration,
    // How long each stop takes, as a Bluetooth headset that is slow to let
    // go. The stream counts as open until it is over.
    pub close_time: Duration,
}

impl Default for Setup {
    fn default() -> Setup {
        Setup {
            period_frames: 128,
            buffer_frames: 256,
            channels: 2,
            rate: 48_000,
            stream_latency: Duration::from_micros(1500),
            signal: |_, _| 0.0,
            open_time: Duration::ZERO,
            close_time: Duration::ZERO,
        }
    }
}

// What a test holds: the devices that are plugged in, which one is the
// default, and a record of what the thread did.
#[derive(Clone)]
pub struct Fake {
    state: Arc<State>,
}

struct State {
    setup: Setup,
    devices: Mutex<Vec<(String, String)>>,
    default: Mutex<Option<String>>,
    wake: Mutex<Option<Wake>>,
    // Every device stops sending events, as a hung driver would.
    stalled: AtomicBool,
    // The next packet captured comes after lost sound.
    glitch: AtomicBool,
    record: Mutex<Record>,
}

#[derive(Clone, Debug, Default)]
pub struct Record {
    // Each open that worked: when, and which device.
    pub opens: Vec<(Instant, String)>,
    // Frames in each packet delivered, or in each write after the first.
    pub packets: Vec<u32>,
    // Everything written to a render stream after the silence it starts
    // with, as interleaved samples.
    pub written: Vec<f32>,
    pub starts: usize,
    pub stops: usize,
    // Streams open or being opened now, and the most there ever were.
    pub at_once: usize,
    pub most_at_once: usize,
}

impl Fake {
    pub fn new(setup: Setup, devices: &[(&str, &str)], default: Option<&str>) -> Fake {
        Fake {
            state: Arc::new(State {
                setup,
                devices: Mutex::new(
                    devices
                        .iter()
                        .map(|(id, name)| (id.to_string(), name.to_string()))
                        .collect(),
                ),
                default: Mutex::new(default.map(str::to_owned)),
                wake: Mutex::new(None),
                stalled: AtomicBool::new(false),
                glitch: AtomicBool::new(false),
                record: Mutex::new(Record::default()),
            }),
        }
    }

    // For Capture::start_with and Render::start_with.
    pub fn device(
        &self,
    ) -> impl FnOnce(Direction, Wake) -> Result<FakeDevice, AudioError> + Send + 'static + use<>
    {
        let state = Arc::clone(&self.state);
        move |direction, wake| {
            *lock(&state.wake) = Some(wake);
            Ok(FakeDevice { state, direction })
        }
    }

    // The device goes away. As on Windows, a stream on it stops being
    // signalled, and only fails when it is read, written or checked. The
    // default stays as it was: Windows' notice of a new one arrives on
    // another thread in no fixed order, so a test sends it with set_default
    // when it wants it.
    pub fn unplug(&self, id: &str) {
        lock(&self.state.devices).retain(|(have, _)| have != id);
    }

    pub fn plug(&self, id: &str, name: &str) {
        lock(&self.state.devices).push((id.to_owned(), name.to_owned()));
    }

    pub fn set_default(&self, id: Option<&str>) {
        *lock(&self.state.default) = id.map(str::to_owned);
        self.tell(id.map(str::to_owned));
    }

    pub fn stall(&self, stalled: bool) {
        self.state.stalled.store(stalled, Ordering::Relaxed);
    }

    // The next packet captured is marked as coming after lost sound.
    pub fn glitch(&self) {
        self.state.glitch.store(true, Ordering::Relaxed);
    }

    pub fn record(&self) -> Record {
        lock(&self.state.record).clone()
    }

    // True for two handles on the same fake devices.
    pub fn same_as(&self, other: &Fake) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    // As voice::audio::default_id for Windows.
    pub fn default_id(&self) -> Option<String> {
        lock(&self.state.default).clone()
    }

    fn tell(&self, id: Option<String>) {
        if let Some(wake) = lock(&self.state.wake).as_ref() {
            wake.default_changed(id);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct FakeDevice {
    state: Arc<State>,
    direction: Direction,
}

impl Device for FakeDevice {
    type Stream = FakeStream;

    fn open(&mut self, choice: &Choice) -> Result<FakeStream, AudioError> {
        {
            let mut record = lock(&self.state.record);
            record.at_once += 1;
            record.most_at_once = record.most_at_once.max(record.at_once);
        }
        thread::sleep(self.state.setup.open_time);
        let opened = self.find(choice);
        if opened.is_err() {
            lock(&self.state.record).at_once -= 1;
        }
        opened
    }
}

impl FakeDevice {
    fn find(&self, choice: &Choice) -> Result<FakeStream, AudioError> {
        let devices = lock(&self.state.devices).clone();
        let id = match choice {
            Choice::Default => lock(&self.state.default)
                .clone()
                .ok_or(AudioError::NoDevice(self.direction))?,
            Choice::Device(id) => id.clone(),
        };
        let Some((_, name)) = devices.iter().find(|(have, _)| *have == id) else {
            return Err(AudioError::NotConnected {
                direction: self.direction,
                name: None,
            });
        };
        let setup = self.state.setup;
        let format = Format {
            rate: setup.rate,
            channels: setup.channels,
            sample: Sample::F32,
            channel_mask: 0,
        };
        lock(&self.state.record)
            .opens
            .push((Instant::now(), id.clone()));
        Ok(FakeStream {
            state: Arc::clone(&self.state),
            info: StreamInfo {
                direction: self.direction,
                device: name.clone(),
                id,
                format,
                engine: format,
                hands_free: false,
                period: Duration::from_secs_f64(
                    f64::from(setup.period_frames) / f64::from(setup.rate),
                ),
                period_frames: setup.period_frames,
                resampled: false,
                small_period: true,
                stream_latency: setup.stream_latency,
                buffer_frames: setup.buffer_frames,
                pro_audio: false,
            },
            started: None,
            ticks_seen: 0,
            frames_out: 0,
            written: 0,
            primed: false,
        })
    }
}

pub struct FakeStream {
    state: Arc<State>,
    info: StreamInfo,
    started: Option<Instant>,
    // Periods handed out so far.
    ticks_seen: u64,
    // Capture: frames delivered so far.
    frames_out: u64,
    // Render: frames written so far, the first silence included.
    written: u64,
    primed: bool,
}

impl FakeStream {
    fn ticks_now(&self) -> u64 {
        let Some(started) = self.started else {
            return 0;
        };
        (started.elapsed().as_secs_f64() / self.info.period.as_secs_f64()) as u64
    }

    fn plugged_in(&self) -> bool {
        lock(&self.state.devices)
            .iter()
            .any(|(id, _)| *id == self.info.id)
    }

    fn present(&self) -> Result<(), AudioError> {
        if self.plugged_in() {
            Ok(())
        } else {
            Err(AudioError::Lost {
                direction: self.info.direction,
                name: self.info.device.clone(),
            })
        }
    }
}

impl Drop for FakeStream {
    fn drop(&mut self) {
        lock(&self.state.record).at_once -= 1;
    }
}

impl DeviceStream for FakeStream {
    fn info(&self) -> &StreamInfo {
        &self.info
    }

    fn start(&mut self) -> Result<(), AudioError> {
        self.present()?;
        self.started = Some(Instant::now());
        lock(&self.state.record).starts += 1;
        Ok(())
    }

    fn wait(&mut self, timeout: Duration) -> Result<bool, AudioError> {
        let Some(started) = self.started else {
            thread::sleep(timeout);
            return Ok(false);
        };
        if self.state.stalled.load(Ordering::Relaxed) || !self.plugged_in() {
            thread::sleep(timeout);
            return Ok(false);
        }
        let next = started + self.info.period.mul_f64((self.ticks_seen + 1) as f64);
        let now = Instant::now();
        if next > now + timeout {
            thread::sleep(timeout);
            return Ok(false);
        }
        thread::sleep(next.saturating_duration_since(now));
        if !self.plugged_in() {
            return Ok(false);
        }
        self.ticks_seen = self.ticks_now().max(self.ticks_seen + 1);
        Ok(true)
    }

    fn check(&mut self) -> Result<(), AudioError> {
        self.present()
    }

    fn read(&mut self, packet: &mut dyn FnMut(Packet<'_>)) -> Result<(), AudioError> {
        self.present()?;
        let setup = self.state.setup;
        let period = u64::from(setup.period_frames);
        let started = self.started.unwrap_or_else(Instant::now);
        while self.frames_out + period <= self.ticks_seen * period {
            let first = self.frames_out;
            let mut data = Vec::with_capacity(self.info.format.frame_bytes() * period as usize);
            for frame in first..first + period {
                for channel in 0..setup.channels {
                    data.extend_from_slice(&(setup.signal)(frame, channel).to_le_bytes());
                }
            }
            let time = started + Duration::from_secs_f64(first as f64 / f64::from(setup.rate));
            // Windows may mark the first packet after a start as coming
            // after lost sound; the fake always does.
            let glitch = first == 0 || self.state.glitch.swap(false, Ordering::Relaxed);
            packet(Packet {
                data: &data,
                frames: setup.period_frames,
                time: Some(time),
                silent: false,
                glitch,
            });
            lock(&self.state.record).packets.push(setup.period_frames);
            self.frames_out += period;
        }
        Ok(())
    }

    fn queued(&mut self) -> Result<u32, AudioError> {
        self.present()?;
        let played = self.ticks_now() * u64::from(self.state.setup.period_frames);
        Ok(self.written.saturating_sub(played) as u32)
    }

    fn write(&mut self, frames: u32, fill: &mut dyn FnMut(&mut [u8])) -> Result<(), AudioError> {
        self.present()?;
        let queued = self.queued()?;
        if queued + frames > self.info.buffer_frames {
            return Err(AudioError::Windows {
                step: String::from("write to the fake device"),
                code: 0x8889_0006,
                text: format!(
                    "{frames} frames do not fit beside {queued} of {}",
                    self.info.buffer_frames
                ),
            });
        }
        let mut bytes = vec![0u8; frames as usize * self.info.format.frame_bytes()];
        fill(&mut bytes);
        // The silence a stream starts with is not what the test asked for.
        if self.primed {
            let mut record = lock(&self.state.record);
            record.packets.push(frames);
            let (samples, _) = bytes.as_chunks::<4>();
            record
                .written
                .extend(samples.iter().map(|b| f32::from_le_bytes(*b)));
        }
        self.primed = true;
        self.written += u64::from(frames);
        Ok(())
    }

    fn stop(&mut self) {
        lock(&self.state.record).stops += 1;
        thread::sleep(self.state.setup.close_time);
    }
}
