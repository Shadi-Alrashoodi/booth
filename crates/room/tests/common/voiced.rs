// Members with fake microphones and speakers that play a signal, for the
// tests that talk: no test opens a real microphone or plays a sound. The
// fakes run on the real clock at the 128-frame (2.7 ms) period of a good
// driver unless a test asks for another.

use std::f64::consts::TAU;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use keys::Identity;
use room::view::{LinkState, View};
use room::{Config, Devices, TalkMode, Timers, VoiceConfig};
use voice::audio::fake::{Fake, Setup};

use super::{Member, config, poll};

// Each test runs audio threads on the real clock, and two at once would
// measure each other.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

pub fn alone() -> MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner)
}

pub const RATE: u64 = 48_000;
pub const PERIOD: u32 = 128;

pub fn frames(count: u64) -> Duration {
    Duration::from_nanos(count * 1_000_000_000 / RATE)
}

pub fn sine(frame: u64, hz: f64, amplitude: f64) -> f32 {
    (amplitude * (TAU * hz * frame as f64 / RATE as f64).sin()) as f32
}

pub fn silence(_: u64, _: u16) -> f32 {
    0.0
}

pub fn tone_440(frame: u64, _: u16) -> f32 {
    sine(frame, 440.0, 0.2)
}

pub fn tone_660(frame: u64, _: u16) -> f32 {
    sine(frame, 660.0, 0.2)
}

fn device(signal: fn(u64, u16) -> f32, id: &str, name: &str, period: u32, close: Duration) -> Fake {
    let setup = Setup {
        period_frames: period,
        buffer_frames: 2 * period,
        signal,
        close_time: close,
        ..Setup::default()
    };
    Fake::new(setup, &[(id, name)], Some(id))
}

// A member's config with a fake microphone playing `signal` and fake
// speakers, and the two fakes to look at.
pub fn voiced(
    name: &str,
    timers: Timers,
    signal: fn(u64, u16) -> f32,
    talk: TalkMode,
    constant_rate: bool,
) -> (Config, Fake, Fake) {
    voiced_at(name, timers, signal, talk, constant_rate, PERIOD)
}

pub fn voiced_at(
    name: &str,
    timers: Timers,
    signal: fn(u64, u16) -> f32,
    talk: TalkMode,
    constant_rate: bool,
    period: u32,
) -> (Config, Fake, Fake) {
    let microphone = device(signal, "mic", "Test microphone", period, Duration::ZERO);
    let speakers = device(silence, "out", "Test speakers", period, Duration::ZERO);
    let mut config = config(name, timers);
    config.voice = VoiceConfig {
        devices: Devices::Fake {
            microphone: microphone.clone(),
            speakers: speakers.clone(),
        },
        talk,
        constant_rate,
        ..VoiceConfig::default()
    };
    (config, microphone, speakers)
}

// Push to talk on a microphone and speakers that each take `close` to stop,
// as a Bluetooth headset that is slow to let go.
pub fn slow_to_close(
    name: &str,
    timers: Timers,
    signal: fn(u64, u16) -> f32,
    close: Duration,
) -> (Config, Fake, Fake) {
    let microphone = device(signal, "mic", "Test microphone", PERIOD, close);
    let speakers = device(silence, "out", "Test speakers", PERIOD, close);
    let (mut config, _, _) = voiced(name, timers, signal, TalkMode::PushToTalk, true);
    config.voice.devices = Devices::Fake {
        microphone: microphone.clone(),
        speakers: speakers.clone(),
    };
    (config, microphone, speakers)
}

pub struct Voiced {
    pub member: Member,
    pub microphone: Fake,
    pub speakers: Fake,
}

impl Voiced {
    pub fn host(name: &str, timers: Timers, signal: fn(u64, u16) -> f32) -> Voiced {
        Voiced::host_at(name, timers, signal, PERIOD)
    }

    pub fn host_at(name: &str, timers: Timers, signal: fn(u64, u16) -> f32, period: u32) -> Voiced {
        let (config, microphone, speakers) =
            voiced_at(name, timers, signal, TalkMode::PushToTalk, true, period);
        Voiced {
            member: Member::host_with(config),
            microphone,
            speakers,
        }
    }

    pub fn host_with(config: (Config, Fake, Fake)) -> Voiced {
        let (config, microphone, speakers) = config;
        Voiced {
            member: Member::host_with(config),
            microphone,
            speakers,
        }
    }

    pub fn join(config: (Config, Fake, Fake), invite: invite::Invite) -> Voiced {
        let (config, microphone, speakers) = config;
        Voiced {
            member: Member::join_with(config, Arc::new(Identity::generate()), invite),
            microphone,
            speakers,
        }
    }

    pub fn view(&self) -> View {
        self.member.view()
    }

    pub fn room(&self) -> &room::Room {
        self.member.room()
    }

    // What the speakers were given, the first channel only, and when the
    // first of it played, on the fake's own clock: after the two periods of
    // silence a stream starts with, which the fake does not record.
    pub fn heard(&self) -> (Vec<f32>, Instant) {
        let record = self.speakers.record();
        let opened = record.opens.first().expect("the speakers opened").0;
        let mono = record.written.iter().step_by(2).copied().collect();
        (mono, opened + frames(2 * u64::from(PERIOD)))
    }

    // When the microphone's first frame was captured, on the fake's clock.
    // The room opens it on its voice devices thread, which a busy PC can
    // hold back past the moment the links settle.
    pub fn first_captured(&self) -> Instant {
        poll(Duration::from_secs(3), "the microphone opened", || {
            self.microphone.record().opens.first().map(|open| open.0)
        })
    }
}

// Everyone live with a clock offset on every link, so every frame from here
// on has a mouth-to-ear time.
pub fn settled(host: &Voiced, clients: &[&Voiced]) {
    let people = clients.len() + 1;
    for client in clients {
        client
            .member
            .wait_for(Duration::from_secs(3), "client settled", |v| {
                v.strip.state == LinkState::Live
                    && v.people.len() == people
                    && v.numbers.clock_offset_ms.is_some()
            });
    }
    host.member
        .wait_for(Duration::from_secs(3), "host settled", |v| {
            v.people.len() == people && v.people.iter().all(|p| p.is_you || p.rtt_ms.is_some())
        });
}

pub fn talking(view: &View, name: &str) -> bool {
    view.people
        .iter()
        .any(|person| person.name == name && !person.is_you && person.talking)
}

pub fn you_talk(view: &View) -> bool {
    view.people
        .iter()
        .any(|person| person.is_you && person.talking)
}

pub fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

// Quieter than this is silence on these speakers, and a gap of this many
// samples of it ends one sound: longer than any gap inside the share cue,
// where one note dies away and the next comes in.
const QUIET: f32 = 1e-3;
const GAP: usize = 480;

// One sound heard on fake speakers that heard nothing else: the share cue.
#[derive(Debug)]
pub struct Cue {
    // Where it starts and how long it is, in samples of the first channel.
    pub at: usize,
    pub len: usize,
    pub peak: f32,
    // Its second half crosses zero more often: the higher note is last.
    pub rising: bool,
}

// Every sound these speakers finished playing, oldest first. One still
// playing, less than GAP from the end of what was written, is left out.
pub fn cues(speakers: &Fake) -> Vec<Cue> {
    let mono: Vec<f32> = speakers
        .record()
        .written
        .iter()
        .step_by(2)
        .copied()
        .collect();
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (at, _) in mono.iter().enumerate().filter(|(_, s)| s.abs() > QUIET) {
        match runs.last_mut() {
            Some((_, last)) if at - *last <= GAP => *last = at,
            _ => runs.push((at, at)),
        }
    }
    if runs
        .last()
        .is_some_and(|&(_, last)| mono.len() - last <= GAP)
    {
        runs.pop();
    }
    let crossings = |part: &[f32]| {
        part.windows(2)
            .filter(|pair| (pair[0] < 0.0) != (pair[1] < 0.0))
            .count()
    };
    runs.into_iter()
        .map(|(first, last)| {
            let sound = &mono[first..=last];
            let (early, late) = sound.split_at(sound.len() / 2);
            Cue {
                at: first,
                len: sound.len(),
                peak: sound.iter().fold(0.0, |most: f32, s| most.max(s.abs())),
                rising: crossings(late) > crossings(early),
            }
        })
        .collect()
}

// Waits up to 3 s for `count` sounds, and gives what was heard by then.
pub fn heard_cues(speakers: &Fake, count: usize) -> Vec<Cue> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let heard = cues(speakers);
        if heard.len() >= count || Instant::now() >= deadline {
            return heard;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
