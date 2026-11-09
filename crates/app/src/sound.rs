// The panel's side of audio. While the settings screen shows: both device
// lists, kept current, the chosen microphone open for the level meter, and
// its rate and connection asked of Windows for the warning under the list.
// In a room the room opens the devices itself. Settings over it keeps the
// lists and asks Windows about the picked microphone, opening no stream, and
// the stats panel asks the devices for their period each time it opens, for
// a side whose stream is closed, as a muted microphone's is. Nothing here
// opens a device at any other time.

use std::mem;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Instant;

use eframe::egui::Context;
use voice::audio::{
    self, AudioError, Capture, Choice, Direction, Echo, Effects, Event, Level, Lists, Microphone,
    Probe, Reading, Render, StreamInfo, Watch,
};
use voice::process::{Playback, Processor, Switches};

use crate::messages;

pub struct SettingsAudio {
    watch: Result<Watch, String>,
    mic: Option<Mic>,
    check: Check,
    // As the watch last read them, taken once a frame by follow.
    lists: Option<Result<Lists, String>>,
    // Hear yourself: what the meter's microphone gives, played on the
    // output device while the switch is on. Never saved: it is off each
    // time the screen opens.
    playback: Playback,
    // What the meter's microphone goes through, as the screen's switches
    // are now.
    switches: Arc<Switches>,
    speakers: Option<(Choice, Render)>,
    speakers_problem: Arc<Mutex<Option<String>>>,
}

impl SettingsAudio {
    pub fn open(ctx: &Context, input: &Choice) -> SettingsAudio {
        let repaint = ctx.clone();
        let watch = Watch::start(move || repaint.request_repaint())
            .map_err(|err| messages::sentence(&err.to_string()));
        // The meter reads the level of what the processing gives, the same
        // as a room's; the samples go to Hear yourself, which drops them
        // while it is off.
        let playback = Playback::default();
        let feed = playback.clone();
        let open: Open = Arc::new(move |choice, echo, processor: Processor, events| {
            let (audio, events) = meter_audio(processor, feed.clone(), events);
            match echo {
                Some(reference) => Capture::start_cancelling_echo(choice, reference, audio, events),
                None => Capture::start(choice, audio, events),
            }
        });
        // A device that will not answer gets no warning; the meter's own
        // line says what is wrong with it.
        let ask: Ask = Arc::new(|choice| {
            let probe = audio::probe(Direction::Input, choice).ok()?;
            Some(Microphone::new(&probe.engine, probe.hands_free))
        });
        let switches = Arc::new(Switches::new(false, true));
        SettingsAudio {
            watch,
            mic: Some(Mic::new(ctx, open, Arc::clone(&switches), input)),
            check: Check::new(ctx, ask, input),
            lists: None,
            playback,
            switches,
            speakers: None,
            speakers_problem: Arc::default(),
        }
    }

    pub fn in_room(ctx: &Context, input: &Choice) -> SettingsAudio {
        let repaint = ctx.clone();
        let watch = Watch::start(move || repaint.request_repaint())
            .map_err(|err| messages::sentence(&err.to_string()));
        let ask: Ask = Arc::new(|choice| {
            let probe = audio::probe(Direction::Input, choice).ok()?;
            Some(Microphone::new(&probe.engine, probe.hands_free))
        });
        SettingsAudio {
            watch,
            mic: None,
            check: Check::new(ctx, ask, input),
            lists: None,
            playback: Playback::default(),
            switches: Arc::new(Switches::new(false, true)),
            speakers: None,
            speakers_problem: Arc::default(),
        }
    }

    pub fn switches(&self) -> &Switches {
        &self.switches
    }

    #[cfg(test)]
    pub(crate) fn has_microphone(&self) -> bool {
        self.mic.is_some()
    }

    // Once a frame while the screen is up, and with `showing` false on each
    // pass while the window is minimized and no frame is drawn. The meter
    // follows the device picked on the screen and is closed while the window
    // is minimized, and a stream that ended starts again when the device
    // lists change, which is when its device may be back. Hear yourself
    // follows the output device, and stops with the meter or once it heard
    // itself. With `echo` the meter's microphone asks Windows for echo
    // cancellation, as a room would, except while Hear yourself plays: the
    // canceller would take the voice coming back as echo and cancel the voice
    // itself.
    pub fn follow(&mut self, input: &Choice, output: &Choice, echo: bool, showing: bool) {
        match &self.speakers {
            Some(_) if !showing || self.playback.tripped() => self.hear(false, output),
            Some((playing, _)) if playing != output => self.hear(true, output),
            _ => {}
        }
        let wish = (echo && !self.hearing()).then(|| output.id().map(str::to_owned));
        let opened = self.mic.as_mut().is_some_and(|mic| {
            mic.ask_echo(wish, showing);
            mic.follow(input, showing)
        });
        self.check.follow(input, opened);
        let lists = match &self.watch {
            Ok(watch) => watch
                .lists()
                .map(|lists| lists.map_err(|err| messages::sentence(&err.to_string()))),
            Err(problem) => Some(Err(problem.clone())),
        };
        if lists != self.lists {
            let changed = matches!(self.lists, Some(Ok(_)));
            self.lists = lists;
            if changed {
                if let Some(mic) = &mut self.mic {
                    mic.retry();
                }
                self.check.ask(false);
            }
        }
    }

    // The chosen microphone as Windows describes it, once it has answered.
    pub fn microphone(&self) -> Option<Microphone> {
        self.check.microphone()
    }

    pub fn lists(&self) -> Option<&Result<Lists, String>> {
        self.lists.as_ref()
    }

    // Why the meter's microphone is not running, until it runs.
    pub fn problem(&self) -> Option<&str> {
        self.mic.as_ref()?.problem.as_deref()
    }

    // None while no stream runs.
    pub fn level(&self) -> Option<Reading> {
        self.mic.as_ref()?.level()
    }

    // What the meter's microphone opened with, while it runs.
    pub fn echo(&self) -> Option<Echo> {
        self.mic.as_ref()?.info().map(|info| info.echo)
    }

    pub fn effects(&self) -> Option<Effects> {
        self.mic.as_ref()?.info().map(|info| info.effects)
    }

    pub fn hearing(&self) -> bool {
        self.speakers.is_some()
    }

    // The speakers open and close on a thread of their own; neither waits
    // here.
    pub fn hear(&mut self, on: bool, output: &Choice) {
        if let Some((_, render)) = self.speakers.take() {
            render.let_go();
        }
        // Off keeps a trip, so the line under the switch still says why.
        self.playback.set_on(on);
        *lock(&self.speakers_problem) = None;
        if !on || self.mic.is_none() {
            return;
        }
        let playback = self.playback.clone();
        let problem = Arc::clone(&self.speakers_problem);
        let events = move |event| {
            *lock(&problem) = match event {
                Event::Opened(_) => None,
                Event::Lost(err) | Event::Gone(err) | Event::Failed(err) => {
                    Some(messages::sentence(&err.to_string()))
                }
            };
        };
        match Render::start(output.clone(), move |out| playback.fill(out), events) {
            Ok(render) => self.speakers = Some((output.clone(), render)),
            Err(err) => {
                self.playback.set_on(false);
                *lock(&self.speakers_problem) = Some(messages::sentence(&err.to_string()));
            }
        }
    }

    // Hear yourself turned itself off: the microphone heard the speakers.
    pub fn tripped(&self) -> bool {
        self.playback.tripped()
    }

    pub fn hear_problem(&self) -> Option<String> {
        lock(&self.speakers_problem).clone()
    }
}

// Closing the screen never waits for a headset slow to let go.
impl Drop for SettingsAudio {
    fn drop(&mut self) {
        if let Some((_, render)) = self.speakers.take() {
            render.let_go();
        }
    }
}

type Events = Box<dyn FnMut(Event) + Send>;
// The choice, and echo cancellation asked for against the given output
// device (None inside for Windows default), or not asked for.
type Open = Arc<
    dyn Fn(Choice, Option<Option<String>>, Processor, Events) -> Result<Capture, AudioError>
        + Send
        + Sync,
>;

// The meter's microphone: each packet through the processing and on to Hear
// yourself, each event on to the meter. When the stream opens its device
// again after a gap it starts afresh, as a room's does: every event clears
// the level the meter draws, and the first packet after Opened restarts the
// processing. Both run on the stream's thread, which says a device went away
// before it shows one open again, so no reading holds sound from before the
// gap. Only Windows default moving to another device goes unsaid, and there
// the old level can show for the instant before Opened.
fn meter_audio(
    mut processor: Processor,
    feed: Playback,
    mut events: Events,
) -> (
    impl FnMut(&[f32], Instant) + Send + 'static,
    impl FnMut(Event) + Send + 'static,
) {
    let level = processor.level();
    let fresh = Arc::new(AtomicBool::new(false));
    let opened = Arc::clone(&fresh);
    let audio = move |samples: &[f32], at: Instant| {
        if fresh.swap(false, Ordering::Relaxed) {
            processor.restart();
        }
        let (out, _) = processor.run(samples, at);
        feed.push(out);
    };
    let events = move |event: Event| {
        level.clear();
        if let Event::Opened(_) = &event {
            opened.store(true, Ordering::Relaxed);
        }
        events(event);
    };
    (audio, events)
}

enum Stream {
    Off,
    // Handed over by the thread that starts it, with its processing's level.
    // Also holds a stop until its thread has ended.
    Starting(Receiver<Result<(Capture, Arc<Level>), AudioError>>),
    On(Capture, Arc<Level>),
}

impl Stream {
    // Returns once the stream's thread has ended.
    fn close(self) {
        match self {
            Stream::Off => {}
            Stream::Starting(started) => {
                if let Ok(Ok((capture, _))) = started.recv() {
                    capture.stop();
                }
            }
            Stream::On(capture, _) => capture.stop(),
        }
    }
}

// The microphone behind the meter. Stopping a stream waits for its thread,
// which may be opening a Bluetooth headset, and that can take a second while
// the headset changes profile. So streams are stopped and started on a
// thread of their own, never the panel's, and each of those threads closes
// the stream before it first, so there is never more than one.
struct Mic {
    ctx: Context,
    open: Open,
    switches: Arc<Switches>,
    echo: Option<Option<String>>,
    choice: Choice,
    stream: Stream,
    // The window is minimized, and the microphone closed until it shows.
    hidden: bool,
    // Counts every start and stop. Events carry the count of the start they
    // came from, so whatever a replaced stream says is dropped, and a start
    // that was replaced before its thread got to it opens nothing.
    turn: Arc<AtomicU64>,
    events: Receiver<(u64, Event)>,
    events_in: Sender<(u64, Event)>,
    problem: Option<String>,
    // The stream ended and will not come back by itself.
    ended: bool,
}

impl Mic {
    fn new(ctx: &Context, open: Open, switches: Arc<Switches>, choice: &Choice) -> Mic {
        let (events_in, events) = mpsc::channel();
        let mut mic = Mic {
            ctx: ctx.clone(),
            open,
            switches,
            echo: None,
            choice: choice.clone(),
            stream: Stream::Off,
            hidden: false,
            turn: Arc::default(),
            events,
            events_in,
            problem: None,
            ended: false,
        };
        mic.replace(true);
        mic
    }

    // True when the stream opened since the last frame.
    fn follow(&mut self, choice: &Choice, showing: bool) -> bool {
        if !showing {
            if !self.hidden {
                self.hidden = true;
                self.replace(false);
            }
            return false;
        }
        if self.hidden || *choice != self.choice {
            self.hidden = false;
            self.choice = choice.clone();
            self.replace(true);
            return false;
        }
        self.take_news()
    }

    // Echo cancellation asked for or not, against which output device: the
    // microphone opens again when that changes, or with the window back when
    // it is minimized or just going.
    fn ask_echo(&mut self, echo: Option<Option<String>>, showing: bool) {
        if echo != self.echo {
            self.echo = echo;
            if showing && !self.hidden {
                self.replace(true);
            }
        }
    }

    // The device lists changed, so a device that went away may be back.
    fn retry(&mut self) {
        if self.ended && !self.hidden {
            self.replace(true);
        }
    }

    fn replace(&mut self, start: bool) {
        let turn = self.turn.fetch_add(1, Ordering::Relaxed) + 1;
        self.problem = None;
        self.ended = false;
        let before = mem::replace(&mut self.stream, Stream::Off);
        let (started_in, started) = mpsc::channel();
        let latest = Arc::clone(&self.turn);
        let open = Arc::clone(&self.open);
        let switches = Arc::clone(&self.switches);
        let echo = self.echo.clone();
        let choice = self.choice.clone();
        let events_in = self.events_in.clone();
        let repaint = self.ctx.clone();
        let spawned = thread::Builder::new()
            .name(String::from("microphone meter"))
            .spawn(move || {
                before.close();
                if !start || latest.load(Ordering::Relaxed) != turn {
                    return;
                }
                let on_event = repaint.clone();
                let events: Events = Box::new(move |event| {
                    let _ = events_in.send((turn, event));
                    on_event.request_repaint();
                });
                let processor = Processor::new(switches);
                let level = processor.level();
                let opened = open(choice, echo, processor, events);
                let _ = started_in.send(opened.map(|on| (on, level)));
                repaint.request_repaint();
            });
        match spawned {
            // A stop hands nothing over, but its sender goes when its thread
            // ends, so a start right after it waits for the stream closing.
            Ok(_) => self.stream = Stream::Starting(started),
            // `before` went down with the thread's closure, and was stopped
            // here as it dropped.
            Err(err) if start => {
                let err = AudioError::Thread(err.to_string());
                self.problem = Some(messages::sentence(&err.to_string()));
                self.ended = true;
            }
            Err(_) => {}
        }
    }

    fn take_news(&mut self) -> bool {
        let mut opened = false;
        if let Stream::Starting(started) = &self.stream {
            match started.try_recv() {
                Ok(Ok((capture, level))) => self.stream = Stream::On(capture, level),
                Ok(Err(err)) => {
                    self.stream = Stream::Off;
                    self.problem = Some(messages::sentence(&err.to_string()));
                    self.ended = true;
                }
                Err(TryRecvError::Empty) => {}
                // Only a panic on that thread ends it without an answer.
                Err(TryRecvError::Disconnected) => {
                    self.stream = Stream::Off;
                    self.ended = true;
                }
            }
        }
        let turn = self.turn.load(Ordering::Relaxed);
        while let Ok((from, event)) = self.events.try_recv() {
            if from != turn {
                continue;
            }
            match event {
                Event::Opened(_) => {
                    self.problem = None;
                    self.ended = false;
                    opened = true;
                }
                // Either way the stream opens the device again by itself.
                Event::Lost(err) | Event::Gone(err) => {
                    self.problem = Some(messages::sentence(&err.to_string()));
                }
                Event::Failed(err) => {
                    // With no microphone at all under Windows default the
                    // stream waits for one by itself.
                    let waits =
                        self.choice == Choice::Default && matches!(err, AudioError::NoDevice(_));
                    self.ended = !waits;
                    self.problem = Some(messages::sentence(&err.to_string()));
                }
            }
        }
        opened
    }

    fn level(&self) -> Option<Reading> {
        let Stream::On(capture, level) = &self.stream else {
            return None;
        };
        capture.info()?;
        Some(level.read())
    }

    fn info(&self) -> Option<StreamInfo> {
        let Stream::On(capture, _) = &self.stream else {
            return None;
        };
        capture.info()
    }
}

impl Drop for Mic {
    fn drop(&mut self) {
        self.replace(false);
    }
}

type Ask = Arc<dyn Fn(&Choice) -> Option<Microphone> + Send + Sync>;

// The chosen microphone as Windows describes it, for the warning under the
// input list. Asking opens no stream, but a device can take a moment to
// answer, so each question runs on a thread of its own. Asked when the
// choice changes, when the device lists change, and when the meter's stream
// opens, since what a Bluetooth headset reports can change once its
// microphone is open, and the open one is what a call gets.
struct Check {
    ctx: Context,
    ask: Ask,
    choice: Choice,
    answer: Arc<Mutex<Answer>>,
}

// Only the answer to the latest question is kept.
#[derive(Default)]
struct Answer {
    turn: u64,
    microphone: Option<Microphone>,
}

impl Check {
    fn new(ctx: &Context, ask: Ask, choice: &Choice) -> Check {
        let mut check = Check {
            ctx: ctx.clone(),
            ask,
            choice: choice.clone(),
            answer: Arc::default(),
        };
        check.ask(true);
        check
    }

    fn follow(&mut self, choice: &Choice, opened: bool) {
        if *choice != self.choice {
            self.choice = choice.clone();
            self.ask(true);
        } else if opened {
            self.ask(false);
        }
    }

    // `forget` for a new device, whose warning must not be the last one's.
    // Otherwise the last answer stays up until the new one is in, so the
    // warning does not blink.
    fn ask(&mut self, forget: bool) {
        let turn = {
            let mut answer = lock(&self.answer);
            if forget {
                answer.microphone = None;
            }
            answer.turn += 1;
            answer.turn
        };
        let answer = Arc::clone(&self.answer);
        let ask = Arc::clone(&self.ask);
        let choice = self.choice.clone();
        let repaint = self.ctx.clone();
        // A thread that cannot start leaves no warning, and the next change
        // asks again.
        let _ = thread::Builder::new()
            .name(String::from("microphone check"))
            .spawn(move || {
                let microphone = ask(&choice);
                let mut answer = lock(&answer);
                if answer.turn == turn {
                    answer.microphone = microphone;
                    repaint.request_repaint();
                }
            });
    }

    fn microphone(&self) -> Option<Microphone> {
        lock(&self.answer).microphone
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// Where each side's number comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum Side {
    Period { ms: f64, resampled: bool },
    NoDevice,
    // The device would not say.
    NotKnown,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Periods {
    pub input: Side,
    pub output: Side,
}

fn side(probe: Result<Probe, AudioError>) -> Side {
    match probe {
        Ok(probe) => Side::Period {
            ms: probe.period.as_secs_f64() * 1000.0,
            resampled: probe.resampled,
        },
        Err(AudioError::NoDevice(_) | AudioError::NotConnected { .. }) => Side::NoDevice,
        Err(_) => Side::NotKnown,
    }
}

// Asked on its own thread, since the devices can take a moment to answer
// and the panel should not wait on them. Asked again each time the stats
// panel opens, since a headset may have connected or become the default in
// between; the last answer stays up until the new one is in.
#[derive(Default)]
pub struct PeriodsAsk {
    asked: bool,
    answer: Arc<Mutex<Option<Periods>>>,
}

impl PeriodsAsk {
    pub fn ask(&mut self, ctx: &Context, devices: &[Choice; 2]) {
        if self.asked {
            return;
        }
        self.asked = true;
        let [input, output] = devices.clone();
        let answer = Arc::clone(&self.answer);
        let ctx = ctx.clone();
        // A thread that cannot start leaves the last answer, or none, and
        // the next opening of the stats panel tries again.
        let _ = thread::Builder::new()
            .name(String::from("audio periods"))
            .spawn(move || {
                let periods = Periods {
                    input: side(audio::probe(Direction::Input, &input)),
                    output: side(audio::probe(Direction::Output, &output)),
                };
                *answer.lock().unwrap_or_else(PoisonError::into_inner) = Some(periods);
                ctx.request_repaint();
            });
    }

    // The stats panel closed; the next time it opens, the devices are asked
    // again.
    pub fn closed(&mut self) {
        self.asked = false;
    }

    pub fn periods(&self) -> Option<Periods> {
        self.answer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::{Duration, Instant};

    use voice::audio::fake::{Fake, Setup};

    use super::*;

    fn fake_open(fake: &Fake) -> Open {
        let fake = fake.clone();
        Arc::new(move |choice, _echo, processor: Processor, events| {
            let (audio, events) = meter_audio(processor, Playback::default(), events);
            Capture::start_with(fake.device(), choice, audio, events)
        })
    }

    // The screen's audio part on a fake device, without the device watch.
    pub(crate) fn fake_audio(ctx: &Context, fake: &Fake, input: &Choice) -> SettingsAudio {
        let switches = Arc::new(Switches::new(false, true));
        SettingsAudio {
            watch: Err(String::from("no device watch")),
            mic: Some(Mic::new(ctx, fake_open(fake), Arc::clone(&switches), input)),
            check: Check::new(ctx, Arc::new(|_: &Choice| None), input),
            lists: None,
            playback: Playback::default(),
            switches,
            speakers: None,
            speakers_problem: Arc::default(),
        }
    }

    // Hear yourself turned on, playing on the fake device.
    pub(crate) fn fake_hear(audio: &mut SettingsAudio, fake: &Fake, output: &Choice) {
        audio.playback.set_on(true);
        let playback = audio.playback.clone();
        let render = Render::start_with(
            fake.device(),
            output.clone(),
            move |out| playback.fill(out),
            |_| {},
        )
        .unwrap();
        audio.speakers = Some((output.clone(), render));
    }

    #[test]
    fn room_settings_open_no_audio_stream() {
        let ctx = Context::default();
        let mut audio = SettingsAudio::in_room(&ctx, &Choice::Default);
        audio.follow(&Choice::Default, &Choice::Default, true, true);
        audio.hear(true, &Choice::Default);
        assert!(audio.mic.is_none());
        assert!(audio.speakers.is_none());
        assert!(audio.level().is_none());
        assert!(audio.problem().is_none());
        assert!(audio.echo().is_none());
        assert!(audio.effects().is_none());
    }

    // Frames as the panel draws them, every 5 ms, until `done`.
    fn frames_until(mic: &mut Mic, choice: &Choice, done: impl Fn(&Mic) -> bool) {
        let until = Instant::now() + Duration::from_secs(3);
        while !done(mic) {
            assert!(Instant::now() < until, "not within 3 s");
            thread::sleep(Duration::from_millis(5));
            mic.follow(choice, true);
        }
    }

    pub(crate) fn wait_until(what: &str, done: impl Fn() -> bool) {
        let until = Instant::now() + Duration::from_secs(1);
        while !done() {
            assert!(Instant::now() < until, "{what}: not within 1 s");
            thread::sleep(Duration::from_millis(2));
        }
    }

    // Moving through the input list while a Bluetooth headset takes its time
    // to open: no frame waits for it, a device passed over is never opened,
    // and there are never two streams at once.
    #[test]
    fn slow_open_never_holds_up_the_panel() {
        let fake = Fake::new(
            Setup {
                open_time: Duration::from_millis(400),
                ..Setup::default()
            },
            &[("a", "Headset A"), ("b", "Headset B"), ("c", "Headset C")],
            Some("a"),
        );
        let mut mic = Mic::new(
            &Context::default(),
            fake_open(&fake),
            Arc::new(Switches::new(false, true)),
            &Choice::Default,
        );
        wait_until("a is opening", || fake.record().at_once == 1);
        let mut slowest = Duration::ZERO;
        for id in ["b", "c"] {
            thread::sleep(Duration::from_millis(20));
            let frame = Instant::now();
            mic.follow(&Choice::Device(String::from(id)), true);
            slowest = slowest.max(frame.elapsed());
        }
        let c = Choice::Device(String::from("c"));
        frames_until(&mut mic, &c, |mic| mic.level().is_some());
        println!("slowest frame {:.2} ms", slowest.as_secs_f64() * 1000.0);
        assert!(slowest < Duration::from_millis(50), "{slowest:?}");
        let record = fake.record();
        let opened: Vec<&str> = record.opens.iter().map(|(_, id)| id.as_str()).collect();
        assert_eq!(opened, ["a", "c"]);
        assert_eq!(record.starts, 1);
        assert_eq!(record.most_at_once, 1);
    }

    // The window is minimized: it shows no meter, so the microphone closes,
    // and opens again when the window comes back.
    #[test]
    fn the_microphone_closes_while_the_window_is_minimized() {
        let fake = Fake::new(Setup::default(), &[("a", "Headset A")], Some("a"));
        let mut mic = Mic::new(
            &Context::default(),
            fake_open(&fake),
            Arc::new(Switches::new(false, true)),
            &Choice::Default,
        );
        frames_until(&mut mic, &Choice::Default, |mic| mic.level().is_some());
        mic.follow(&Choice::Default, false);
        assert!(mic.level().is_none());
        wait_until("the stream closes", || fake.record().at_once == 0);
        thread::sleep(Duration::from_millis(50));
        mic.follow(&Choice::Default, false);
        assert_eq!(fake.record().opens.len(), 1);
        frames_until(&mut mic, &Choice::Default, |mic| mic.level().is_some());
        assert_eq!(fake.record().opens.len(), 2);
        drop(mic);
        wait_until("the stream closes after the screen", || {
            fake.record().at_once == 0
        });
    }

    // Minimized and back while a Bluetooth headset is still opening: the new
    // stream waits for the old one to close, so there are never two at once.
    #[test]
    fn a_quick_restore_waits_for_the_stream_closing() {
        let fake = Fake::new(
            Setup {
                open_time: Duration::from_millis(400),
                ..Setup::default()
            },
            &[("a", "Headset A")],
            Some("a"),
        );
        let mut mic = Mic::new(
            &Context::default(),
            fake_open(&fake),
            Arc::new(Switches::new(false, true)),
            &Choice::Default,
        );
        wait_until("the stream is opening", || fake.record().at_once == 1);
        mic.follow(&Choice::Default, false);
        mic.follow(&Choice::Default, true);
        frames_until(&mut mic, &Choice::Default, |mic| mic.level().is_some());
        let record = fake.record();
        assert_eq!(record.opens.len(), 2);
        assert_eq!(record.most_at_once, 1);
    }

    // Stopping Hear yourself on the pass that first sees the window minimized
    // changes what the microphone asks for. That pass only stops the
    // microphone, and the wish waits for the window to come back.
    #[test]
    fn a_minimize_starts_nothing_and_the_echo_wish_waits_for_the_window() {
        let fake = Fake::new(Setup::default(), &[("a", "Headset A")], Some("a"));
        let input = Choice::Default;
        let output = Choice::Device(String::from("a"));
        let mut audio = fake_audio(&Context::default(), &fake, &input);
        fake_hear(&mut audio, &fake, &output);
        wait_until("both streams open", || fake.record().at_once == 2);
        let echoes = Arc::new(Mutex::new(Vec::new()));
        let plain = fake_open(&fake);
        let asked = Arc::clone(&echoes);
        audio.mic.as_mut().unwrap().open = Arc::new(
            move |choice, echo: Option<Option<String>>, processor, events| {
                lock(&asked).push(echo.clone());
                plain(choice, echo, processor, events)
            },
        );

        let turn = audio.mic.as_ref().unwrap().turn.load(Ordering::Relaxed);
        audio.follow(&input, &output, true, false);
        assert!(!audio.hearing());
        assert_eq!(
            audio.mic.as_ref().unwrap().turn.load(Ordering::Relaxed),
            turn + 1
        );
        wait_until("both streams close", || fake.record().at_once == 0);
        assert!(lock(&echoes).is_empty());

        audio.follow(&input, &output, true, true);
        wait_until("the microphone opens", || !lock(&echoes).is_empty());
        assert_eq!(*lock(&echoes), [Some(Some(String::from("a")))]);
    }

    // A device that is not connected fails a moment after it was passed
    // over. That is about the old choice, and nothing is shown under the new
    // one while it opens.
    #[test]
    fn a_replaced_stream_says_nothing_about_the_new_choice() {
        let fake = Fake::new(
            Setup {
                open_time: Duration::from_millis(200),
                ..Setup::default()
            },
            &[("b", "Headset B")],
            Some("b"),
        );
        let a = Choice::Device(String::from("a"));
        let b = Choice::Device(String::from("b"));
        let mut mic = Mic::new(
            &Context::default(),
            fake_open(&fake),
            Arc::new(Switches::new(false, true)),
            &a,
        );
        wait_until("a is opening", || fake.record().at_once == 1);
        mic.follow(&b, true);
        let mut said = None;
        let until = Instant::now() + Duration::from_secs(3);
        while mic.level().is_none() {
            assert!(Instant::now() < until, "b did not open within 3 s");
            thread::sleep(Duration::from_millis(5));
            mic.follow(&b, true);
            said = said.or(mic.problem.clone());
        }
        assert_eq!(said, None);
        assert!(!mic.ended);
    }

    // A microphone chosen before and not plugged in: the meter says why, and
    // opens it once it is plugged in, by itself and only once, without a
    // new stream from the list change that came with the plug.
    #[test]
    fn a_chosen_microphone_opens_once_it_is_plugged_in() {
        let fake = Fake::new(Setup::default(), &[("b", "Headset B")], Some("b"));
        let pods = Choice::Device(String::from("pods"));
        let mut mic = Mic::new(
            &Context::default(),
            fake_open(&fake),
            Arc::new(Switches::new(false, true)),
            &pods,
        );
        frames_until(&mut mic, &pods, |mic| mic.problem.is_some());
        assert!(!mic.ended);
        fake.plug("pods", "Headset");
        mic.retry();
        frames_until(&mut mic, &pods, |mic| mic.level().is_some());
        assert_eq!(mic.problem, None);
        let record = fake.record();
        assert_eq!(record.opens.len(), 1);
        assert_eq!(record.most_at_once, 1);
    }

    // Full scale until the microphone goes away, silence once it is back in
    // the same stream: the meter shows nothing from before, neither while
    // the stream shows it open before saying so, nor on the frame that hears
    // it opened, nor once the first packet is in. Packets of 25 ms leave the
    // panel time to look in between.
    #[test]
    fn the_meter_starts_empty_when_the_microphone_comes_back() {
        static LOUD: AtomicBool = AtomicBool::new(true);
        let fake = Fake::new(
            Setup {
                period_frames: 1_200,
                buffer_frames: 2_400,
                signal: |_, _| {
                    if LOUD.load(Ordering::Relaxed) {
                        1.0
                    } else {
                        0.0
                    }
                },
                ..Setup::default()
            },
            &[("mic", "Headset")],
            Some("mic"),
        );
        // What the meter would draw as each Opened leaves the stream, before
        // the meter hears it.
        let shown = Arc::new(Mutex::new(Vec::new()));
        let open: Open = {
            let fake = fake.clone();
            let shown = Arc::clone(&shown);
            Arc::new(move |choice, _echo, processor: Processor, events| {
                let level = processor.level();
                let shown = Arc::clone(&shown);
                let (audio, mut events) = meter_audio(processor, Playback::default(), events);
                let events = move |event: Event| {
                    if let Event::Opened(_) = &event {
                        lock(&shown).push(level.read());
                    }
                    events(event);
                };
                Capture::start_with(fake.device(), choice, audio, events)
            })
        };
        let choice = Choice::Device(String::from("mic"));
        let mut mic = Mic::new(
            &Context::default(),
            open,
            Arc::new(Switches::new(false, false)),
            &choice,
        );
        frames_until(&mut mic, &choice, |mic| {
            mic.level().is_some_and(|level| level.peak == 1.0)
        });
        fake.unplug("mic");
        frames_until(&mut mic, &choice, |mic| mic.problem.is_some());
        LOUD.store(false, Ordering::Relaxed);
        let packets = fake.record().packets.len();
        fake.plug("mic", "Headset");
        let until = Instant::now() + Duration::from_secs(3);
        while !mic.follow(&choice, true) {
            assert!(Instant::now() < until, "not open again within 3 s");
            thread::sleep(Duration::from_millis(1));
        }
        let opened = mic.level();
        wait_until("the first packet", || fake.record().packets.len() > packets);
        let first = mic.level();
        let again = lock(&shown).get(1).copied();
        for reading in [again, opened, first] {
            let reading = reading.unwrap();
            assert_eq!((reading.peak, reading.rms), (0.0, 0.0), "{reading:?}");
        }
    }

    // A slow answer about a device that was passed over never shows under the
    // one chosen after it, and a new choice takes the old warning away at
    // once. The meter opening asks again without taking it away.
    #[test]
    fn the_warning_is_about_the_device_chosen_now() {
        const AIRPODS: Microphone = Microphone {
            rate: 8_000,
            hands_free: true,
        };
        const USB: Microphone = Microphone {
            rate: 48_000,
            hands_free: false,
        };
        let ask: Ask = Arc::new(|choice: &Choice| match choice.id() {
            Some("airpods") => Some(AIRPODS),
            Some("slow") => {
                thread::sleep(Duration::from_millis(300));
                Some(AIRPODS)
            }
            Some("usb") => Some(USB),
            _ => None,
        });
        let device = |id: &str| Choice::Device(String::from(id));
        let mut check = Check::new(&Context::default(), ask, &device("airpods"));
        wait_until("the airpods answer", || check.microphone() == Some(AIRPODS));

        check.follow(&device("usb"), false);
        assert_ne!(check.microphone(), Some(AIRPODS));
        wait_until("the usb answer", || check.microphone() == Some(USB));

        check.follow(&device("slow"), false);
        check.follow(&device("usb"), false);
        wait_until("the usb answer again", || check.microphone() == Some(USB));
        thread::sleep(Duration::from_millis(400));
        assert_eq!(check.microphone(), Some(USB));

        check.follow(&device("usb"), true);
        assert_eq!(check.microphone(), Some(USB));
    }
}
